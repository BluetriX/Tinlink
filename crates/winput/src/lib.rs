//! Listen-only keyboard and mouse state for push-to-talk.
//!
//! A registered global hotkey (`RegisterHotKey`) swallows the key, so a game
//! in the foreground would no longer see it. Polling the asynchronous key
//! state only reads it: the key still reaches every other program, and mouse
//! buttons (e.g. the side buttons) work as well.

use std::fmt;
use std::str::FromStr;

/// A key or mouse button, as a Windows virtual-key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key(u8);

const CTRL: u8 = 0x11;
const SHIFT: u8 = 0x10;
const ALT: u8 = 0x12;

/// Names for keys that are not a single letter, digit or `F<n>`.
const NAMES: &[(&str, u8)] = &[
    ("Mouse3", 0x04),
    ("Mouse4", 0x05),
    ("Mouse5", 0x06),
    ("Backspace", 0x08),
    ("Tab", 0x09),
    ("Enter", 0x0D),
    ("Shift", SHIFT),
    ("Ctrl", CTRL),
    ("Alt", ALT),
    ("Pause", 0x13),
    ("CapsLock", 0x14),
    ("Escape", 0x1B),
    ("Space", 0x20),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("End", 0x23),
    ("Home", 0x24),
    ("Left", 0x25),
    ("Up", 0x26),
    ("Right", 0x27),
    ("Down", 0x28),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("NumMultiply", 0x6A),
    ("NumAdd", 0x6B),
    ("NumSubtract", 0x6D),
    ("NumDecimal", 0x6E),
    ("NumDivide", 0x6F),
    ("ScrollLock", 0x91),
];

/// Codes that are never a push-to-talk key or only stand for a side of a
/// modifier (the generic Ctrl/Shift/Alt codes are used instead).
fn ignored(code: u8) -> bool {
    matches!(code, 0x00..=0x03 | 0x07 | 0x0A..=0x0C | 0x0E | 0x0F | 0xA0..=0xA5 | 0xFF)
}

fn is_modifier(code: u8) -> bool {
    matches!(code, CTRL | SHIFT | ALT)
}

impl Key {
    /// For window interaction (not allowed as push-to-talk key).
    pub const LEFT_BUTTON: Key = Key(0x01);
    pub const ESCAPE: Key = Key(0x1B);
    pub const F11: Key = Key(0x7A);

    pub fn code(self) -> u8 {
        self.0
    }

    /// Whether the key or button is held right now.
    pub fn is_down(self) -> bool {
        key_down(self.0)
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.0;
        if let Some((name, _)) = NAMES.iter().find(|(_, code)| *code == c) {
            return f.write_str(name);
        }
        match c {
            0x30..=0x39 | 0x41..=0x5A => write!(f, "{}", c as char),
            0x60..=0x69 => write!(f, "Num{}", c - 0x60),
            0x70..=0x87 => write!(f, "F{}", c - 0x70 + 1),
            _ => write!(f, "0x{c:02X}"),
        }
    }
}

impl FromStr for Key {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let unknown = || format!("unknown key {s:?}");
        if let Some((_, code)) = NAMES.iter().find(|(name, _)| name.eq_ignore_ascii_case(s)) {
            return Ok(Key(*code));
        }
        let lower = s.to_ascii_lowercase();
        let code = match lower.as_str() {
            "control" => CTRL,
            "esc" => 0x1B,
            "mouse1" | "mouse2" => {
                return Err("the left and right mouse buttons cannot be used".into());
            }
            _ if s.len() == 1 && s.chars().all(|c| c.is_ascii_alphanumeric()) => {
                s.to_ascii_uppercase().as_bytes()[0]
            }
            _ if lower.starts_with("num") => {
                let n: u8 = lower[3..].parse().map_err(|_| unknown())?;
                if n > 9 {
                    return Err(unknown());
                }
                0x60 + n
            }
            _ if lower.starts_with("0x") => {
                u8::from_str_radix(&lower[2..], 16).map_err(|_| unknown())?
            }
            _ if lower.starts_with('f') => {
                let n: u8 = lower[1..].parse().map_err(|_| unknown())?;
                if !(1..=24).contains(&n) {
                    return Err(unknown());
                }
                0x70 + n - 1
            }
            _ => return Err(unknown()),
        };
        if ignored(code) {
            return Err(unknown());
        }
        Ok(Key(code))
    }
}

/// One key or button, optionally with modifiers, e.g. `F8`, `Mouse4` or
/// `Ctrl+Shift+Space`. Active while all of its keys are held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotkey {
    keys: Vec<Key>,
}

impl Hotkey {
    pub fn is_down(&self) -> bool {
        self.keys.iter().all(|k| k.is_down())
    }

    /// The modifiers being held plus the first other key or button pressed
    /// right now, if any (the left and right mouse buttons are ignored, so
    /// clicking a "record" button does not record itself).
    pub fn capture() -> Option<Self> {
        let main = (0x01..=0xFEu8)
            .filter(|&c| !ignored(c) && !is_modifier(c))
            .find(|&c| key_down(c))?;
        let mut keys: Vec<Key> = [CTRL, SHIFT, ALT]
            .into_iter()
            .filter(|&m| key_down(m))
            .map(Key)
            .collect();
        keys.push(Key(main));
        Some(Self { keys })
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, key) in self.keys.iter().enumerate() {
            if i > 0 {
                f.write_str("+")?;
            }
            write!(f, "{key}")?;
        }
        Ok(())
    }
}

impl FromStr for Hotkey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut keys = s
            .split('+')
            .map(str::parse)
            .collect::<Result<Vec<Key>, _>>()?;
        if keys.is_empty() || keys.iter().all(|k| is_modifier(k.0)) && keys.len() > 1 {
            return Err(format!("invalid key combination {s:?}"));
        }
        // Modifiers first, in a fixed order, without duplicates.
        keys.sort_by_key(|k| match k.0 {
            CTRL => 0,
            SHIFT => 1,
            ALT => 2,
            _ => 3,
        });
        keys.dedup();
        Ok(Self { keys })
    }
}

#[cfg(windows)]
fn key_down(code: u8) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    // SAFETY: GetAsyncKeyState only reads global input state; any code is valid.
    let state = unsafe { GetAsyncKeyState(i32::from(code)) };
    // The most significant bit is set while the key is down.
    state < 0
}

#[cfg(not(windows))]
fn key_down(_code: u8) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hk(s: &str) -> Hotkey {
        s.parse().unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn parses_and_formats_keys() {
        for name in [
            "F8", "F24", "Mouse4", "Mouse5", "Space", "A", "7", "Num3", "CapsLock", "0xBA",
        ] {
            assert_eq!(hk(name).to_string(), name);
        }
        assert_eq!(hk("f8").to_string(), "F8");
        assert_eq!(hk(" mouse4 ").to_string(), "Mouse4");
        assert_eq!(hk("esc").to_string(), "Escape");
    }

    #[test]
    fn orders_modifiers_first() {
        assert_eq!(hk("space+shift+ctrl").to_string(), "Ctrl+Shift+Space");
        assert_eq!(hk("Alt+Mouse5").to_string(), "Alt+Mouse5");
        assert_eq!(hk("Ctrl+Ctrl+X").to_string(), "Ctrl+X");
    }

    #[test]
    fn codes_match_windows_virtual_keys() {
        assert_eq!("F8".parse::<Key>().unwrap().code(), 0x77);
        assert_eq!("Mouse4".parse::<Key>().unwrap().code(), 0x05);
        assert_eq!("A".parse::<Key>().unwrap().code(), 0x41);
        assert_eq!("Num0".parse::<Key>().unwrap().code(), 0x60);
    }

    #[test]
    fn rejects_unusable_keys() {
        for bad in [
            "",
            "Mouse1",
            "Mouse2",
            "F25",
            "F0",
            "Num10",
            "Hyper",
            "Ctrl+Shift",
            "0xA0",
            "+",
        ] {
            assert!(bad.parse::<Hotkey>().is_err(), "{bad:?} should be rejected");
        }
        // A lone modifier is allowed (some people use Ctrl as push-to-talk).
        assert_eq!(hk("Ctrl").to_string(), "Ctrl");
    }
}
