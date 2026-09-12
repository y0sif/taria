//! Lowering of taria's key grammar into crossterm key events.
//!
//! The grammar itself lives in [`taria::key`], shared with the bridge so that
//! a key string the bridge accepts is exactly one this adapter can deliver.
//! This module is only the last step: turning a parsed
//! [`KeyPress`](taria::key::KeyPress) into the [`KeyEvent`] a ratatui app
//! already handles.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use taria::key::{Key, KeyPress, Modifiers};

/// Parse a key description from a [`taria::AgentInput::Key`] message into a
/// crossterm [`KeyEvent`] (via ratatui's `crossterm` re-export).
///
/// The accepted syntax is [`taria::key::KEY_GRAMMAR`]. `None` means either that
/// the string does not match it, which is the same verdict the bridge reaches
/// from the same parser, or that [`to_crossterm`] has no crossterm counterpart
/// for the key it parsed to. A caller that already holds a parsed
/// [`KeyPress`](taria::key::KeyPress) should use [`to_crossterm`] rather than
/// render it back to a string to re-parse here.
pub fn to_crossterm_key(key: &str) -> Option<KeyEvent> {
    key.parse::<KeyPress>().ok().and_then(to_crossterm)
}

/// Lower an already-parsed [`KeyPress`] into a crossterm [`KeyEvent`], or
/// `None` for a key this build cannot express.
///
/// It covers every key in [`Key`] as of this version, so `None` means the app
/// was built against a newer taria whose grammar learned a key this adapter
/// has not: the string parsed, and there is no crossterm event to deliver for
/// it. Reported rather than approximated, so the app can ack the input
/// [`Ignored`](taria::wire::InputStatus::Ignored) the way it already does for a
/// key the grammar rejects. Lowering it to some near-miss event would put a
/// keystroke into the app that the agent never asked for and report it as
/// applied.
///
/// [`Key::BackTab`] always arrives from the parser with `shift` set, so it
/// needs no special case: it lowers like any other modified press and comes out
/// as [`KeyCode::BackTab`] with [`KeyModifiers::SHIFT`], which is what a
/// terminal delivers for shift+tab.
pub fn to_crossterm(press: KeyPress) -> Option<KeyEvent> {
    let code = match press.key {
        Key::Char(c) => KeyCode::Char(c),
        Key::Enter => KeyCode::Enter,
        Key::Esc => KeyCode::Esc,
        Key::Tab => KeyCode::Tab,
        Key::BackTab => KeyCode::BackTab,
        Key::Backspace => KeyCode::Backspace,
        Key::Delete => KeyCode::Delete,
        Key::Up => KeyCode::Up,
        Key::Down => KeyCode::Down,
        Key::Left => KeyCode::Left,
        Key::Right => KeyCode::Right,
        Key::Home => KeyCode::Home,
        Key::End => KeyCode::End,
        Key::PageUp => KeyCode::PageUp,
        Key::PageDown => KeyCode::PageDown,
        Key::F(n) => KeyCode::F(n),
        // A key taria added after this adapter was written. Deliberately not
        // mapped to a placeholder such as `KeyCode::Null`: the app would then
        // handle a press nobody sent, and ack it as delivered.
        _ => return None,
    };
    Some(KeyEvent::new(code, to_crossterm_modifiers(press.modifiers)))
}

/// Lower taria's modifier flags into the matching [`KeyModifiers`] bits.
fn to_crossterm_modifiers(modifiers: Modifiers) -> KeyModifiers {
    let mut out = KeyModifiers::NONE;
    if modifiers.ctrl {
        out |= KeyModifiers::CONTROL;
    }
    if modifiers.alt {
        out |= KeyModifiers::ALT;
    }
    if modifiers.shift {
        out |= KeyModifiers::SHIFT;
    }
    out
}

/// Lower literal text into one key event per character, for an app whose
/// typing surface consumes key events, such as a text field.
///
/// Typing a string as one message instead of one round trip per character is
/// the reason [`taria::AgentInput::Text`] exists, so the expansion belongs
/// here, next to an app that already knows how to handle key events.
///
/// It is not the way to handle that variant in every app. Enter and Tab are
/// control keys, which a text field reads as "submit" and "next field", and a
/// surface with no control-key vocabulary receives them from here anyway. A
/// typing tutor whose typing screen grades characters, and binds Tab to a
/// setting it saves to the user's config file, is where this surfaced:
/// lowered through this function, a tab in an agent's text would flip that
/// setting, the very binding trip that routing text away from the key handler
/// is meant to prevent. An app like that iterates `text.chars()` itself and
/// decides what a newline or a tab means on its own surface.
///
/// The rules:
///
/// - `'\n'` becomes [`KeyCode::Enter`], so a multi-line string submits lines
///   the way a person typing it would;
/// - `'\t'` becomes [`KeyCode::Tab`];
/// - `'\r'` is skipped, so text that arrives with CRLF line endings behaves
///   exactly like the same text with LF;
/// - every other character becomes [`KeyCode::Char`] with
///   [`KeyModifiers::NONE`].
///
/// An uppercase character carries no [`KeyModifiers::SHIFT`]: the character
/// itself already says which one it is, and this matches what
/// [`to_crossterm_key`] produces for `"Q"`, so the two paths cannot disagree
/// about the same text.
pub fn text_to_keys(text: &str) -> Vec<KeyEvent> {
    text.chars()
        .filter(|c| *c != '\r')
        .map(|c| {
            let code = match c {
                '\n' => KeyCode::Enter,
                '\t' => KeyCode::Tab,
                other => KeyCode::Char(other),
            };
            KeyEvent::new(code, KeyModifiers::NONE)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: Modifiers = Modifiers::new(true, false, false);
    const SHIFT: Modifiers = Modifiers::new(false, false, true);
    const CTRL_ALT_SHIFT: Modifiers = Modifiers::new(true, true, true);

    fn event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// One case per `Key` variant: the mapping is this module's whole job, so
    /// a missed variant has to fail here.
    #[test]
    fn every_key_lowers_to_its_crossterm_counterpart() {
        let cases = [
            (Key::Char('a'), KeyCode::Char('a')),
            (Key::Char('Q'), KeyCode::Char('Q')),
            (Key::Char(' '), KeyCode::Char(' ')),
            (Key::Enter, KeyCode::Enter),
            (Key::Esc, KeyCode::Esc),
            (Key::Tab, KeyCode::Tab),
            (Key::BackTab, KeyCode::BackTab),
            (Key::Backspace, KeyCode::Backspace),
            (Key::Delete, KeyCode::Delete),
            (Key::Up, KeyCode::Up),
            (Key::Down, KeyCode::Down),
            (Key::Left, KeyCode::Left),
            (Key::Right, KeyCode::Right),
            (Key::Home, KeyCode::Home),
            (Key::End, KeyCode::End),
            (Key::PageUp, KeyCode::PageUp),
            (Key::PageDown, KeyCode::PageDown),
            (Key::F(1), KeyCode::F(1)),
            (Key::F(12), KeyCode::F(12)),
        ];
        for (key, code) in cases {
            assert_eq!(
                to_crossterm(KeyPress::new(key, Modifiers::NONE)),
                Some(event(code, KeyModifiers::NONE)),
                "key: {key:?}"
            );
        }
    }

    #[test]
    fn modifiers_lower_to_the_matching_bits() {
        let cases = [
            (Modifiers::NONE, KeyModifiers::NONE),
            (CTRL, KeyModifiers::CONTROL),
            (SHIFT, KeyModifiers::SHIFT),
            (
                CTRL_ALT_SHIFT,
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
            ),
        ];
        for (modifiers, expected) in cases {
            assert_eq!(
                to_crossterm(KeyPress::new(Key::Char('x'), modifiers)),
                Some(event(KeyCode::Char('x'), expected)),
                "modifiers: {modifiers:?}"
            );
        }
    }

    /// `BackTab` reaches this module with shift already set, and keeps it.
    #[test]
    fn backtab_keeps_its_shift() {
        assert_eq!(
            to_crossterm(KeyPress::new(Key::BackTab, SHIFT)),
            Some(event(KeyCode::BackTab, KeyModifiers::SHIFT))
        );
    }

    /// End to end through the parser. The grammar itself is tested in
    /// `taria::key`; these only prove the string path still reaches the same
    /// events it used to.
    #[test]
    fn strings_parse_and_lower() {
        let cases = [
            ("ctrl+c", event(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            ("shift+tab", event(KeyCode::BackTab, KeyModifiers::SHIFT)),
            ("space", event(KeyCode::Char(' '), KeyModifiers::NONE)),
            (" ", event(KeyCode::Char(' '), KeyModifiers::NONE)),
            ("Q", event(KeyCode::Char('Q'), KeyModifiers::NONE)),
        ];
        for (input, expected) in cases {
            assert_eq!(to_crossterm_key(input), Some(expected), "input: {input:?}");
        }
        assert_eq!(to_crossterm_key("meta+x"), None);
    }

    #[test]
    fn text_becomes_one_event_per_character() {
        assert_eq!(
            text_to_keys("hi!"),
            [
                event(KeyCode::Char('h'), KeyModifiers::NONE),
                event(KeyCode::Char('i'), KeyModifiers::NONE),
                event(KeyCode::Char('!'), KeyModifiers::NONE),
            ]
        );
    }

    #[test]
    fn empty_text_produces_no_events() {
        assert!(text_to_keys("").is_empty());
    }

    #[test]
    fn newlines_and_tabs_become_their_keys() {
        assert_eq!(
            text_to_keys("a\n\tb"),
            [
                event(KeyCode::Char('a'), KeyModifiers::NONE),
                event(KeyCode::Enter, KeyModifiers::NONE),
                event(KeyCode::Tab, KeyModifiers::NONE),
                event(KeyCode::Char('b'), KeyModifiers::NONE),
            ]
        );
    }

    #[test]
    fn crlf_text_matches_the_same_text_with_lf() {
        assert_eq!(text_to_keys("one\r\ntwo"), text_to_keys("one\ntwo"));
        assert!(text_to_keys("\r").is_empty());
    }

    /// Uppercase text must not gain a shift the string path never adds, or an
    /// app matching on `(Char('Q'), NONE)` would see typed text as a
    /// different press than the same character sent as a key.
    #[test]
    fn uppercase_text_carries_no_shift() {
        assert_eq!(
            text_to_keys("Q"),
            [event(KeyCode::Char('Q'), KeyModifiers::NONE)]
        );
        assert_eq!(text_to_keys("Q").first().copied(), to_crossterm_key("Q"));
    }
}
