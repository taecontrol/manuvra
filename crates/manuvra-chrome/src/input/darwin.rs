use super::Key;
use serde_json::{Value, json};

// Chrome on macOS performs native editing for a synthesized key only through
// the Cocoa selector sent with it; `insert*` selectors are left to the text.
const EDITING_COMMANDS: [(Key, &str); 7] = [
    (Key::ArrowUp, "moveUp"),
    (Key::ArrowDown, "moveDown"),
    (Key::ArrowLeft, "moveLeft"),
    (Key::ArrowRight, "moveRight"),
    (Key::Home, "scrollToBeginningOfDocument"),
    (Key::End, "scrollToEndOfDocument"),
    (Key::Escape, "cancelOperation"),
];

pub(super) fn add_editing_commands(key: Key, event: &mut Value) {
    if let Some((_, command)) = EDITING_COMMANDS
        .iter()
        .find(|(candidate, _)| *candidate == key)
    {
        event["commands"] = json!([command]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_and_escape_carry_their_cocoa_editing_command() {
        for (key, expected) in [
            (Key::ArrowUp, Some("moveUp")),
            (Key::ArrowDown, Some("moveDown")),
            (Key::ArrowLeft, Some("moveLeft")),
            (Key::ArrowRight, Some("moveRight")),
            (Key::Home, Some("scrollToBeginningOfDocument")),
            (Key::End, Some("scrollToEndOfDocument")),
            (Key::Escape, Some("cancelOperation")),
            (Key::Enter, None),
            (Key::Tab, None),
            (Key::ShiftTab, None),
            (Key::Space, None),
        ] {
            let mut event = json!({"type":"keyDown"});
            add_editing_commands(key, &mut event);
            assert_eq!(
                event.get("commands"),
                expected.map(|command| json!([command])).as_ref(),
                "{key:?}"
            );
        }
    }
}
