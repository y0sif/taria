//! Parsing of taria's textual key syntax into crossterm key events.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Parse a key description from a [`taria::AgentInput::Key`] message into a
/// crossterm [`KeyEvent`] (via ratatui's `crossterm` re-export).
///
/// Accepted forms, all case-insensitive except the character itself:
///
/// - single characters: `"a"`, `"Q"`, `"?"`, `"+"`;
/// - named keys: `enter`, `esc`, `tab`, `backtab`, `backspace`, `delete`,
///   `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`,
///   `space`, `f1`–`f12` (plus aliases `return`, `escape`, `del`);
/// - modifier prefixes chained with `+`: `"ctrl+c"`, `"alt+enter"`,
///   `"ctrl+shift+p"`. Modifiers are `ctrl` (alias `control`), `alt`,
///   `shift`.
///
/// `"shift+tab"` and `"backtab"` both map to [`KeyCode::BackTab`] with
/// [`KeyModifiers::SHIFT`], matching what terminals deliver. Returns `None`
/// for anything unrecognized.
pub fn to_crossterm_key(key: &str) -> Option<KeyEvent> {
    // A lone space would be destroyed by trimming; catch it first.
    if key == " " {
        return Some(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    }
    let trimmed = key.trim();

    // Peel off modifier prefixes: everything before a '+' that names a
    // modifier. Stops at the base key, so `"ctrl++"` leaves `"+"`.
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = trimmed;
    while let Some(pos) = rest.find('+') {
        if pos == 0 || pos + 1 >= rest.len() {
            break;
        }
        let modifier = match rest[..pos].to_ascii_lowercase().as_str() {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "alt" => KeyModifiers::ALT,
            "shift" => KeyModifiers::SHIFT,
            _ => break,
        };
        modifiers |= modifier;
        rest = &rest[pos + 1..];
    }

    // Single character base: taken literally (case preserved).
    let mut chars = rest.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(KeyEvent::new(KeyCode::Char(c), modifiers));
    }

    let code = match rest.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => {
            if modifiers.contains(KeyModifiers::SHIFT) {
                KeyCode::BackTab
            } else {
                KeyCode::Tab
            }
        }
        "backtab" => {
            modifiers |= KeyModifiers::SHIFT;
            KeyCode::BackTab
        }
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        other => {
            let n: u8 = other.strip_prefix('f')?.parse().ok()?;
            if (1..=12).contains(&n) {
                KeyCode::F(n)
            } else {
                return None;
            }
        }
    };
    Some(KeyEvent::new(code, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Option<KeyEvent> {
        Some(KeyEvent::new(code, modifiers))
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
                to_crossterm_key(input),
                key(KeyCode::Char(expected), KeyModifiers::NONE),
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn named_keys_parse_case_insensitively() {
        let cases = [
            ("enter", KeyCode::Enter),
            ("Enter", KeyCode::Enter),
            ("RETURN", KeyCode::Enter),
            ("esc", KeyCode::Esc),
            ("escape", KeyCode::Esc),
            ("tab", KeyCode::Tab),
            ("backspace", KeyCode::Backspace),
            ("delete", KeyCode::Delete),
            ("del", KeyCode::Delete),
            ("up", KeyCode::Up),
            ("down", KeyCode::Down),
            ("left", KeyCode::Left),
            ("right", KeyCode::Right),
            ("home", KeyCode::Home),
            ("end", KeyCode::End),
            ("pageup", KeyCode::PageUp),
            ("PageDown", KeyCode::PageDown),
            ("space", KeyCode::Char(' ')),
            ("f1", KeyCode::F(1)),
            ("F12", KeyCode::F(12)),
        ];
        for (input, code) in cases {
            assert_eq!(
                to_crossterm_key(input),
                key(code, KeyModifiers::NONE),
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn modifiers_combine() {
        let cases = [
            ("ctrl+c", KeyCode::Char('c'), KeyModifiers::CONTROL),
            ("CTRL+c", KeyCode::Char('c'), KeyModifiers::CONTROL),
            ("control+c", KeyCode::Char('c'), KeyModifiers::CONTROL),
            ("alt+enter", KeyCode::Enter, KeyModifiers::ALT),
            ("shift+f5", KeyCode::F(5), KeyModifiers::SHIFT),
            (
                "ctrl+alt+delete",
                KeyCode::Delete,
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            (
                "ctrl+shift+p",
                KeyCode::Char('p'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            ("ctrl++", KeyCode::Char('+'), KeyModifiers::CONTROL),
        ];
        for (input, code, modifiers) in cases {
            assert_eq!(
                to_crossterm_key(input),
                key(code, modifiers),
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn shift_tab_and_backtab_are_backtab_with_shift() {
        assert_eq!(
            to_crossterm_key("shift+tab"),
            key(KeyCode::BackTab, KeyModifiers::SHIFT)
        );
        assert_eq!(
            to_crossterm_key("backtab"),
            key(KeyCode::BackTab, KeyModifiers::SHIFT)
        );
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
            assert_eq!(to_crossterm_key(input), None, "input: {input:?}");
        }
    }
}
