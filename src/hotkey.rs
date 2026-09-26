//! Hotkey chord spec and the pure state machine the keyboard hook drives.
//! Win32 plumbing lives in `win/hook.rs`; everything here is unit-tested.

use std::fmt;

pub const CTRL: u8 = 1;
pub const ALT: u8 = 2;
pub const SHIFT: u8 = 4;
pub const WIN: u8 = 8;

/// Required modifiers plus an optional non-modifier key (`key == 0` means modifier-only).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Chord {
    pub mods: u8,
    pub key: u16,
}

impl Chord {
    pub const DEFAULT: Chord = Chord {
        mods: CTRL | WIN,
        key: 0,
    };

    pub fn pack(self) -> u64 {
        u64::from(self.mods) | (u64::from(self.key) << 8)
    }

    pub fn unpack(v: u64) -> Chord {
        Chord {
            mods: (v & 0xff) as u8,
            key: ((v >> 8) & 0xffff) as u16,
        }
    }

    /// Parses "Ctrl+Win", "Ctrl+Shift+Space", "Alt+F9". Needs at least one modifier.
    pub fn parse(s: &str) -> Option<Chord> {
        let mut c = Chord { mods: 0, key: 0 };
        for part in s.split('+').map(str::trim) {
            let bit = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => CTRL,
                "alt" => ALT,
                "shift" => SHIFT,
                "win" | "super" => WIN,
                _ => 0,
            };
            if bit != 0 {
                c.mods |= bit;
            } else if c.key == 0 {
                c.key = key_vk(part)?;
            } else {
                return None;
            }
        }
        (c.mods != 0).then_some(c)
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = [(CTRL, "Ctrl"), (ALT, "Alt"), (SHIFT, "Shift"), (WIN, "Win")]
            .iter()
            .filter(|(bit, _)| self.mods & bit != 0)
            .map(|(_, name)| (*name).to_string())
            .collect();
        if self.key != 0 {
            parts.push(key_name(self.key));
        }
        f.write_str(&parts.join("+"))
    }
}

const NAMED_KEYS: &[(&str, u16)] = &[
    ("Space", 0x20),
    ("Enter", 0x0D),
    ("Tab", 0x09),
    ("Esc", 0x1B),
    ("Backspace", 0x08),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("Pause", 0x13),
    ("`", 0xC0),
];

fn key_vk(name: &str) -> Option<u16> {
    if let Some(&(_, vk)) = NAMED_KEYS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
    {
        return Some(vk);
    }
    let upper = name.to_ascii_uppercase();
    let b = upper.as_bytes();
    if b.len() == 1 && b[0].is_ascii_alphanumeric() {
        return Some(u16::from(b[0])); // VK codes for 0-9 and A-Z equal ASCII
    }
    match upper.strip_prefix('F')?.parse::<u16>().ok()? {
        n @ 1..=24 => Some(0x6F + n), // VK_F1 = 0x70
        _ => None,
    }
}

fn key_name(vk: u16) -> String {
    if let Some(&(n, _)) = NAMED_KEYS.iter().find(|(_, v)| *v == vk) {
        return n.to_string();
    }
    match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from(vk as u8).to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        _ => format!("0x{vk:02X}"),
    }
}

/// Modifier bit for a virtual-key code, or 0 for a non-modifier key.
pub fn mod_bit(vk: u16) -> u8 {
    match vk {
        0x10 | 0xA0 | 0xA1 => SHIFT,
        0x11 | 0xA2 | 0xA3 => CTRL,
        0x12 | 0xA4 | 0xA5 => ALT,
        0x5B | 0x5C => WIN,
        _ => 0,
    }
}

/// Left/right VK pairs per modifier bit, for `GetAsyncKeyState` resync.
pub const MOD_KEYS: [(u8, u16, u16); 4] = [
    (CTRL, 0xA2, 0xA3),
    (ALT, 0xA4, 0xA5),
    (SHIFT, 0xA0, 0xA1),
    (WIN, 0x5B, 0x5C),
];

/// Modifier set after this event. The event's own key comes from the event; every other
/// modifier key is resynced from `is_down` (GetAsyncKeyState), so a key-up the hook missed
/// (Win+L, UAC prompt) can't leave the chord stuck.
pub fn mods_after(vk: u16, down: bool, is_down: impl Fn(u16) -> bool) -> u8 {
    let mut mods = 0;
    for (bit, l, r) in MOD_KEYS {
        let held = if vk == l || vk == r {
            let other = if vk == l { r } else { l };
            down || is_down(other)
        } else {
            is_down(l) || is_down(r)
        };
        if held {
            mods |= bit;
        }
    }
    mods
}

/// What the hook should do with the current event.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Out {
    /// Post a toggle to the app.
    pub toggle: bool,
    /// Inject a dummy key before passing this Win key-up on, so Start doesn't open.
    pub mask_win: bool,
    /// Swallow this event (only the key of a key chord).
    pub swallow: bool,
}

#[derive(Default)]
pub struct ChordMachine {
    armed: bool,
    dirty: bool,
    key_held: bool,
}

impl ChordMachine {
    /// Feed one non-injected key event. `mods` is `mods_after(..)` for this event.
    pub fn feed(&mut self, chord: Chord, vk: u16, down: bool, mods: u8) -> Out {
        let mut out = Out::default();
        let is_mod = mod_bit(vk) != 0;

        if chord.key != 0 {
            if vk == chord.key {
                if down && !self.key_held && mods == chord.mods {
                    out.toggle = true;
                }
                // Swallow the whole press (including repeats and the up) once it fired.
                out.swallow = self.key_held || out.toggle;
                self.key_held = down && out.swallow;
            }
            return out;
        }

        // Modifier-only chord: fires on release, only if nothing else happened in between.
        if down && !is_mod {
            self.dirty = true;
        }
        if mods & !chord.mods != 0 {
            self.dirty = true; // an extra modifier joined in
        }
        if mods == chord.mods && !self.dirty {
            self.armed = true;
        }
        if !down && is_mod && self.armed && !self.dirty {
            if mod_bit(vk) == WIN && chord.mods & WIN != 0 {
                out.mask_win = true;
            }
            if mods & chord.mods == 0 {
                out.toggle = true;
            }
        }
        if mods & chord.mods == 0 {
            self.armed = false;
        }
        if mods == 0 {
            self.dirty = false;
        }
        out
    }

    /// Forget everything (after hook reinstall, resume, unlock).
    pub fn reset(&mut self) {
        *self = ChordMachine::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LCTRL: u16 = 0xA2;
    const RCTRL: u16 = 0xA3;
    const LWIN: u16 = 0x5B;
    const LSHIFT: u16 = 0xA0;
    const RIGHT: u16 = 0x27;
    const D: u16 = 0x44;

    /// Replays (vk, down) events tracking held keys like GetAsyncKeyState would.
    /// Returns (toggles, win masks).
    fn run(chord: Chord, events: &[(u16, bool)]) -> (u32, u32) {
        let mut m = ChordMachine::default();
        let mut held: Vec<u16> = Vec::new();
        let (mut toggles, mut masks) = (0, 0);
        for &(vk, down) in events {
            let mods = mods_after(vk, down, |k| held.contains(&k));
            let out = m.feed(chord, vk, down, mods);
            toggles += u32::from(out.toggle);
            masks += u32::from(out.mask_win);
            held.retain(|&k| k != vk);
            if down {
                held.push(vk);
            }
        }
        (toggles, masks)
    }

    const CW: Chord = Chord::DEFAULT;

    #[test]
    fn clean_chord_fires_once_on_release() {
        assert_eq!(
            run(
                CW,
                &[(LCTRL, true), (LWIN, true), (LWIN, false), (LCTRL, false)]
            ),
            (1, 1)
        );
        assert_eq!(
            run(
                CW,
                &[(LWIN, true), (LCTRL, true), (LCTRL, false), (LWIN, false)]
            ),
            (1, 1)
        );
    }

    #[test]
    fn autorepeat_does_not_matter() {
        let ev = [
            (LCTRL, true),
            (LWIN, true),
            (LWIN, true),
            (LWIN, true),
            (LCTRL, true),
            (LCTRL, false),
            (LWIN, false),
        ];
        assert_eq!(run(CW, &ev).0, 1);
    }

    #[test]
    fn right_ctrl_counts() {
        assert_eq!(
            run(
                CW,
                &[(RCTRL, true), (LWIN, true), (RCTRL, false), (LWIN, false)]
            )
            .0,
            1
        );
    }

    #[test]
    fn other_key_makes_it_dirty() {
        // Ctrl+Win+Right switches virtual desktop; must not toggle.
        let ev = [
            (LCTRL, true),
            (LWIN, true),
            (RIGHT, true),
            (RIGHT, false),
            (LWIN, false),
            (LCTRL, false),
        ];
        assert_eq!(run(CW, &ev), (0, 0));
        let ev = [
            (LWIN, true),
            (LCTRL, true),
            (D, true),
            (D, false),
            (LCTRL, false),
            (LWIN, false),
        ];
        assert_eq!(run(CW, &ev).0, 0);
    }

    #[test]
    fn extra_modifier_makes_it_dirty() {
        let ev = [
            (LCTRL, true),
            (LWIN, true),
            (LSHIFT, true),
            (LSHIFT, false),
            (LWIN, false),
            (LCTRL, false),
        ];
        assert_eq!(run(CW, &ev).0, 0);
    }

    #[test]
    fn dirty_clears_after_all_released() {
        let ev = [
            (LCTRL, true),
            (LWIN, true),
            (RIGHT, true),
            (RIGHT, false),
            (LWIN, false),
            (LCTRL, false),
            (LCTRL, true),
            (LWIN, true),
            (LWIN, false),
            (LCTRL, false),
        ];
        assert_eq!(run(CW, &ev).0, 1);
    }

    #[test]
    fn single_modifier_does_nothing() {
        assert_eq!(run(CW, &[(LWIN, true), (LWIN, false)]), (0, 0));
        assert_eq!(
            run(
                CW,
                &[(LCTRL, true), (0x43, true), (0x43, false), (LCTRL, false)]
            ),
            (0, 0)
        );
    }

    #[test]
    fn missed_key_up_recovers_via_async_state() {
        // Win's key-up was never seen (e.g. Win+L), but GetAsyncKeyState says it's up.
        let mut m = ChordMachine::default();
        let none = |_: u16| false;
        m.feed(CW, LCTRL, true, mods_after(LCTRL, true, none));
        m.feed(CW, LWIN, true, mods_after(LWIN, true, |k| k == LCTRL));
        // Later: Ctrl released; resync shows Win up too -> chord completes, then state is clean.
        let out = m.feed(CW, LCTRL, false, mods_after(LCTRL, false, none));
        assert!(out.toggle);
        let only_ctrl = [(LCTRL, true), (LCTRL, false)];
        let mut held = vec![];
        for &(vk, down) in &only_ctrl {
            let o = m.feed(CW, vk, down, mods_after(vk, down, |k| held.contains(&k)));
            assert!(!o.toggle);
            held.push(vk);
        }
    }

    #[test]
    fn key_chord_fires_on_first_down_and_swallows() {
        let c = Chord::parse("Ctrl+Shift+Space").unwrap();
        let mut m = ChordMachine::default();
        let mods = CTRL | SHIFT;
        let a = m.feed(c, 0x20, true, mods);
        let b = m.feed(c, 0x20, true, mods); // repeat
        let up = m.feed(c, 0x20, false, mods);
        assert_eq!((a.toggle, a.swallow), (true, true));
        assert_eq!((b.toggle, b.swallow), (false, true));
        assert_eq!((up.toggle, up.swallow), (false, true));
        let plain = m.feed(c, 0x20, true, 0);
        assert_eq!(plain, Out::default());
    }

    #[test]
    fn parse_format_pack_roundtrip() {
        for s in [
            "Ctrl+Win",
            "Ctrl+Shift+Space",
            "Alt+F9",
            "Ctrl+Alt+K",
            "Win+1",
        ] {
            let c = Chord::parse(s).unwrap();
            assert_eq!(c.to_string(), s);
            assert_eq!(Chord::unpack(c.pack()), c);
        }
        assert_eq!(Chord::parse("win + ctrl"), Some(Chord::DEFAULT));
        assert_eq!(Chord::parse("Space"), None);
        assert_eq!(Chord::parse("Ctrl+A+B"), None);
        assert_eq!(Chord::parse("Ctrl+Nope"), None);
    }
}
