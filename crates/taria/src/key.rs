//! Textual key grammar for [`AgentInput::Key`](crate::AgentInput::Key).
//!
//! The wire carries a key as a plain string, so both peers must agree on what
//! that string means. This module is that agreement: the app side lowers a
//! [`KeyPress`] into its framework's key event, and the bridge parses the same
//! grammar to reject a malformed key before it costs a round trip. Keeping one
//! parser here is what makes the two verdicts identical.
//!
//! These types are deliberately not serde-serializable. The wire form of a key
//! is the string, and adding a second encoding would let the two drift.
//!
//! [`KeyPress`] implements [`FromStr`] and [`Display`](fmt::Display), and the
//! two round-trip: every press the parser can produce renders to a string that
//! parses back to the same press.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

/// Human-readable summary of the key grammar, phrased to follow "expected".
///
/// Shared so the parser's error message and an agent-facing tool description
/// cannot describe different grammars.
pub const KEY_GRAMMAR: &str = "a single character (`a`, `Q`, `?`, `+`), or a named key (enter, \
     esc, tab, backtab, backspace, delete, up, down, left, right, home, end, pageup, pagedown, \
     space, f1 through f12, plus the aliases return, escape, del), optionally prefixed with \
     modifiers joined by `+` (ctrl, alt, shift; `control` is an alias for ctrl). Names and \
     modifiers are case-insensitive, a single character keeps its case. Examples: `q`, `Q`, \
     `ctrl+c`, `alt+enter`, `ctrl+shift+p`, `space`";

/// A key with no modifiers applied, the base of a [`KeyPress`].
///
/// `Char` holds the character verbatim, so case is meaningful: `Char('Q')` and
/// `Char('q')` are different presses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A literal character, including `Char(' ')` for the space bar.
    Char(char),
    Enter,
    Esc,
    Tab,
    /// Backwards tab, what a terminal delivers for shift+tab.
    BackTab,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// Function key, `F(1)` through `F(12)`.
    F(u8),
}

/// Modifier keys held during a [`KeyPress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Modifiers {
    /// No modifiers held.
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
    };
}

/// One key press: a [`Key`] plus the [`Modifiers`] held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPress {
    pub key: Key,
    pub modifiers: Modifiers,
}

impl KeyPress {
    /// Build a press from its parts.
    pub const fn new(key: Key, modifiers: Modifiers) -> Self {
        Self { key, modifiers }
    }
}

/// A key string that does not match the grammar.
///
/// Keeps the rejected input so the message can name it: an agent that guessed
/// a key name needs to see which guess failed to correct itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyParseError {
    input: String,
}

impl KeyParseError {
    /// The input that was rejected, verbatim.
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for KeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unrecognized key `{}`; expected {KEY_GRAMMAR}",
            self.input
        )
    }
}

impl Error for KeyParseError {}

impl FromStr for KeyPress {
    type Err = KeyParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse(s).ok_or_else(|| KeyParseError {
            input: s.to_string(),
        })
    }
}

/// Parse the grammar, or `None` if the input does not match it.
///
/// Split out so [`FromStr`] owns the one place that builds the error, and so
/// every failure path is an early `None` rather than a panic.
fn parse(s: &str) -> Option<KeyPress> {
    // A lone space is a legitimate key and would be destroyed by trimming, so
    // it has to be answered before the trim below.
    if s == " " {
        return Some(KeyPress::new(Key::Char(' '), Modifiers::NONE));
    }
    let trimmed = s.trim();

    // Peel off modifier prefixes: everything before a `+` that names a
    // modifier. A `+` that opens or closes the remainder is the base key
    // rather than a separator, which is what lets `"ctrl++"` mean ctrl plus
    // the `+` character while `"+a"` and `"ctrl+"` stay errors.
    let mut modifiers = Modifiers::NONE;
    let mut rest = trimmed;
    while let Some(pos) = rest.find('+') {
        if pos == 0 || pos + 1 >= rest.len() {
            break;
        }
        // `find` reports a char boundary, so both slices always exist; `get`
        // keeps this function free of indexing that could panic.
        let (name, tail) = (rest.get(..pos)?, rest.get(pos + 1..)?);
        match name.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.ctrl = true,
            "alt" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            _ => break,
        }
        rest = tail;
    }

    // A single-character base is taken literally, case preserved.
    let mut chars = rest.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(KeyPress::new(Key::Char(c), modifiers));
    }

    let key = match rest.to_ascii_lowercase().as_str() {
        "enter" | "return" => Key::Enter,
        "esc" | "escape" => Key::Esc,
        // Terminals deliver shift+tab as a distinct backwards tab, so the two
        // spellings have to land on the same press.
        "tab" => {
            if modifiers.shift {
                Key::BackTab
            } else {
                Key::Tab
            }
        }
        "backtab" => {
            modifiers.shift = true;
            Key::BackTab
        }
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "space" => Key::Char(' '),
        other => {
            let n: u8 = other.strip_prefix('f')?.parse().ok()?;
            if (1..=12).contains(&n) {
                Key::F(n)
            } else {
                return None;
            }
        }
    };
    Some(KeyPress::new(key, modifiers))
}

impl fmt::Display for Key {
    /// Write the base key without modifiers.
    ///
    /// `Char(' ')` renders as `space`, never a literal space, so that a
    /// modified press such as `ctrl+space` stays parseable. `BackTab` renders
    /// as `backtab`, which parses back with shift already set.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Char(' ') => f.write_str("space"),
            Key::Char(c) => write!(f, "{c}"),
            Key::Enter => f.write_str("enter"),
            Key::Esc => f.write_str("esc"),
            Key::Tab => f.write_str("tab"),
            Key::BackTab => f.write_str("backtab"),
            Key::Backspace => f.write_str("backspace"),
            Key::Delete => f.write_str("delete"),
            Key::Up => f.write_str("up"),
            Key::Down => f.write_str("down"),
            Key::Left => f.write_str("left"),
            Key::Right => f.write_str("right"),
            Key::Home => f.write_str("home"),
            Key::End => f.write_str("end"),
            Key::PageUp => f.write_str("pageup"),
            Key::PageDown => f.write_str("pagedown"),
            Key::F(n) => write!(f, "f{n}"),
        }
    }
}

impl fmt::Display for KeyPress {
    /// Write the canonical form: modifiers in ctrl, alt, shift order, then the
    /// base key.
    ///
    /// `BackTab` already implies shift on the way back in, so no `shift+`
    /// prefix is emitted for it. Every press the parser can produce round-trips
    /// through this form; a hand-built `BackTab` with `shift: false` cannot,
    /// because the parser never produces one.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.ctrl {
            f.write_str("ctrl+")?;
        }
        if self.modifiers.alt {
            f.write_str("alt+")?;
        }
        if self.modifiers.shift && self.key != Key::BackTab {
            f.write_str("shift+")?;
        }
        write!(f, "{}", self.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;
    const CTRL: Modifiers = Modifiers {
        ctrl: true,
        alt: false,
        shift: false,
    };
    const ALT: Modifiers = Modifiers {
        ctrl: false,
        alt: true,
        shift: false,
    };
    const SHIFT: Modifiers = Modifiers {
        ctrl: false,
        alt: false,
        shift: true,
    };
    const CTRL_ALT: Modifiers = Modifiers {
        ctrl: true,
        alt: true,
        shift: false,
    };
    const CTRL_SHIFT: Modifiers = Modifiers {
        ctrl: true,
        alt: false,
        shift: true,
    };

    fn press(input: &str) -> Option<KeyPress> {
        input.parse::<KeyPress>().ok()
    }

    #[test]
    fn single_characters_parse_literally() {
        let cases = [
            ("a", 'a'),
            ("Q", 'Q'),
            ("?", '?'),
            ("+", '+'),
            ("/", '/'),
            (" ", ' '),
            ("é", 'é'),
        ];
        for (input, expected) in cases {
            assert_eq!(
                press(input),
                Some(KeyPress::new(Key::Char(expected), NONE)),
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn named_keys_parse_case_insensitively() {
        let cases = [
            ("enter", Key::Enter),
            ("Enter", Key::Enter),
            ("RETURN", Key::Enter),
            ("esc", Key::Esc),
            ("escape", Key::Esc),
            ("tab", Key::Tab),
            ("backspace", Key::Backspace),
            ("delete", Key::Delete),
            ("del", Key::Delete),
            ("up", Key::Up),
            ("down", Key::Down),
            ("left", Key::Left),
            ("right", Key::Right),
            ("home", Key::Home),
            ("end", Key::End),
            ("pageup", Key::PageUp),
            ("PageDown", Key::PageDown),
            ("space", Key::Char(' ')),
            ("f1", Key::F(1)),
            ("F12", Key::F(12)),
        ];
        for (input, key) in cases {
            assert_eq!(
                press(input),
                Some(KeyPress::new(key, NONE)),
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn modifiers_combine() {
        let cases = [
            ("ctrl+c", Key::Char('c'), CTRL),
            ("CTRL+c", Key::Char('c'), CTRL),
            ("control+c", Key::Char('c'), CTRL),
            ("alt+enter", Key::Enter, ALT),
            ("shift+f5", Key::F(5), SHIFT),
            ("ctrl+alt+delete", Key::Delete, CTRL_ALT),
            ("ctrl+shift+p", Key::Char('p'), CTRL_SHIFT),
            ("ctrl++", Key::Char('+'), CTRL),
        ];
        for (input, key, modifiers) in cases {
            assert_eq!(
                press(input),
                Some(KeyPress::new(key, modifiers)),
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn shift_tab_and_backtab_are_backtab_with_shift() {
        assert_eq!(press("shift+tab"), Some(KeyPress::new(Key::BackTab, SHIFT)));
        assert_eq!(press("backtab"), Some(KeyPress::new(Key::BackTab, SHIFT)));
    }

    #[test]
    fn invalid_inputs_are_rejected() {
        let rejects = [
            "",
            "nope",
            "f0",
            "f13",
            "f99",
            "ctrl",
            "ctrl+",
            "+a",
            "meta+x",
            "enterx",
            "ab",
            "ctrl+nope",
        ];
        for input in rejects {
            assert_eq!(press(input), None, "input: {input:?}");
        }
    }

    #[test]
    fn display_roundtrips_through_from_str() {
        // Every variant of `Key`, both spellings of space, the `+` base key,
        // and both spellings of backtab.
        let inputs = [
            "a",
            "Q",
            "?",
            " ",
            "space",
            "é",
            "ctrl++",
            "ctrl+space",
            "enter",
            "esc",
            "tab",
            "backtab",
            "shift+tab",
            "ctrl+backtab",
            "backspace",
            "delete",
            "up",
            "down",
            "left",
            "right",
            "home",
            "end",
            "pageup",
            "pagedown",
            "f1",
            "f12",
            "shift+f5",
            "ctrl+alt+shift+enter",
        ];
        for input in inputs {
            let parsed = press(input).unwrap_or_else(|| panic!("input: {input:?}"));
            let rendered = parsed.to_string();
            assert!(
                !rendered.contains(' ') || rendered == "space",
                "canonical form of {input:?} has a literal space: {rendered:?}"
            );
            assert_eq!(
                press(&rendered),
                Some(parsed),
                "input {input:?} rendered as {rendered:?}"
            );
        }
    }

    #[test]
    fn canonical_form_normalizes_spelling_and_order() {
        let cases = [
            (" ", "space"),
            ("space", "space"),
            ("Q", "Q"),
            ("ctrl++", "ctrl++"),
            ("CTRL+c", "ctrl+c"),
            ("control+c", "ctrl+c"),
            ("backtab", "backtab"),
            ("shift+tab", "backtab"),
            ("ctrl+backtab", "ctrl+backtab"),
            ("shift+alt+ctrl+enter", "ctrl+alt+shift+enter"),
            ("F12", "f12"),
        ];
        for (input, expected) in cases {
            let parsed = press(input).unwrap_or_else(|| panic!("input: {input:?}"));
            assert_eq!(parsed.to_string(), expected, "input: {input:?}");
        }
    }

    #[test]
    fn error_names_the_offending_input() {
        let err = "ctrl+nope".parse::<KeyPress>().unwrap_err();
        assert_eq!(err.input(), "ctrl+nope");
        let message = err.to_string();
        assert!(message.contains("ctrl+nope"), "message: {message}");
        assert!(message.contains("pagedown"), "message: {message}");
    }
}
