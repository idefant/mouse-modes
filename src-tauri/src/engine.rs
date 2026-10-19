use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hotkey {
    pub keys: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Hotkeys {
    pub slow: Option<Hotkey>,
    pub fast: Option<Hotkey>,
    pub scroll_horizontal: Option<Hotkey>,
    pub scroll_vertical: Option<Hotkey>,
    pub scroll_both: Option<Hotkey>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub hotkeys: Hotkeys,
    pub autostart: bool,
}

impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        let bindings = [
            &self.hotkeys.slow,
            &self.hotkeys.fast,
            &self.hotkeys.scroll_horizontal,
            &self.hotkeys.scroll_vertical,
            &self.hotkeys.scroll_both,
        ];
        for binding in bindings.into_iter().flatten() {
            if binding.keys.is_empty() || binding.keys.len() > 16 {
                return Err("A hotkey must contain between 1 and 16 keys".into());
            }
            let mut unique = std::collections::HashSet::new();
            for key in &binding.keys {
                if !supported_key(key) || !unique.insert(key) {
                    return Err(format!("Invalid or duplicate hotkey key: {key}"));
                }
            }
            for modifier in ["Control", "Shift", "Alt", "Meta"] {
                if binding.keys.iter().any(|key| key == modifier)
                    && binding.keys.iter().any(|key| {
                        key == &format!("{modifier}Left") || key == &format!("{modifier}Right")
                    })
                {
                    return Err("An any-side modifier cannot also specify a side".into());
                }
            }
        }
        Ok(())
    }
}

pub fn supported_key(key: &str) -> bool {
    if let Some(letter) = key.strip_prefix("Key") {
        return letter.len() == 1 && letter.as_bytes()[0].is_ascii_uppercase();
    }
    if let Some(digit) = key.strip_prefix("Digit") {
        return digit.len() == 1 && digit.as_bytes()[0].is_ascii_digit();
    }
    if let Some(number) = key.strip_prefix('F') {
        if let Ok(number) = number.parse::<u8>() {
            return (1..=24).contains(&number);
        }
    }
    matches!(
        key,
        "Control"
            | "Shift"
            | "Alt"
            | "Meta"
            | "ControlLeft"
            | "ControlRight"
            | "ShiftLeft"
            | "ShiftRight"
            | "AltLeft"
            | "AltRight"
            | "MetaLeft"
            | "MetaRight"
            | "Escape"
            | "Enter"
            | "Backspace"
            | "Tab"
            | "Space"
            | "CapsLock"
            | "ArrowUp"
            | "ArrowDown"
            | "ArrowLeft"
            | "ArrowRight"
            | "PageUp"
            | "PageDown"
            | "Home"
            | "End"
            | "Insert"
            | "Delete"
            | "NumLock"
            | "ScrollLock"
            | "PrintScreen"
            | "Pause"
            | "Minus"
            | "Equal"
            | "BracketLeft"
            | "BracketRight"
            | "Backslash"
            | "Semicolon"
            | "Quote"
            | "Backquote"
            | "Comma"
            | "Period"
            | "Slash"
            | "ContextMenu"
            | "NumpadEnter"
            | "NumpadAdd"
            | "NumpadSubtract"
            | "NumpadMultiply"
            | "NumpadDivide"
            | "NumpadDecimal"
            | "Numpad0"
            | "Numpad1"
            | "Numpad2"
            | "Numpad3"
            | "Numpad4"
            | "Numpad5"
            | "Numpad6"
            | "Numpad7"
            | "Numpad8"
            | "Numpad9"
    )
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStatus {
    pub available: bool,
    pub message: String,
    pub capturing: bool,
}

#[derive(Clone, Debug)]
pub enum EngineEvent {
    Captured(Hotkey),
    Status(EngineStatus),
}

pub type EventCallback = Arc<dyn Fn(EngineEvent) + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_disable_every_mode() {
        let settings = Settings::default();
        assert!(settings.validate().is_ok());
        assert_eq!(
            serde_json::to_value(settings).unwrap(),
            serde_json::json!({
                "hotkeys": {"slow":null,"fast":null,"scrollHorizontal":null,"scrollVertical":null,"scrollBoth":null},
                "autostart":false
            })
        );
    }

    #[test]
    fn sides_and_generic_modifiers_are_valid_but_duplicates_are_not() {
        let mut settings = Settings::default();
        settings.hotkeys.slow = Some(Hotkey {
            keys: vec!["ControlLeft".into(), "KeyA".into()],
        });
        assert!(settings.validate().is_ok());
        settings
            .hotkeys
            .slow
            .as_mut()
            .unwrap()
            .keys
            .push("Control".into());
        assert!(settings.validate().is_err());
        settings.hotkeys.slow = Some(Hotkey {
            keys: vec!["F13".into(), "F13".into()],
        });
        assert!(settings.validate().is_err());
        settings.hotkeys.slow = Some(Hotkey {
            keys: vec!["KeyЖ".into()],
        });
        assert!(settings.validate().is_err());
    }
}
