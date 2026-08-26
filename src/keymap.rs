use crate::config::config_directory;
use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rhai::{Engine, ImmutableString};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::fs;

const DEFAULT_KEYMAP: &str = r#"
map("global", "f1", "help");
map("global", "f2", "settings");
map("global", "f3", "model");
map("global", "f4", "status");
map("global", "ctrl+,", "settings");
map("global", "ctrl+p", "protocols");
map("global", "ctrl+t", "tasks");
map("global", "ctrl+shift+c", "copy");
map("global", "super+c", "copy");
map("global", "esc", "interrupt_on_double_press");

map("main", "space", "compose");
map("main", "@", "reference");
map("main", "ctrl+v", "paste");
map("main", "alt+v", "paste_image");
map("main", ":", "command");
map("main", "?", "help");
map("main", "r", "jump_reasoning");
map("main", "t", "jump_tools");
map("main", "h", "jump_user");
map("main", "down", "next");
map("main", "up", "previous");
map("main", "j", "next");
map("main", "k", "previous");
map("main", "pagedown", "page_down");
map("main", "pageup", "page_up");
map("main", "ctrl+down", "scroll_down");
map("main", "ctrl+up", "scroll_up");
map("main", "home", "first");
map("main", "end", "last");
map("main", "enter", "toggle");
map("main", "o", "open");
map("main", "esc", "clear");
map("main", "y", "copy");

map("composer", "enter", "submit");
map("composer", "shift+enter", "newline");
map("composer", "ctrl+enter", "newline");
map("composer", "ctrl+j", "newline");
map("composer", "up", "cursor_up");
map("composer", "shift+up", "cursor_up");
map("composer", "down", "cursor_down");
map("composer", "shift+down", "cursor_down");
map("composer", "ctrl+home", "first");
map("composer", "ctrl+shift+home", "first");
map("composer", "ctrl+end", "last");
map("composer", "ctrl+shift+end", "last");
map("composer", "alt+left", "word_back");
map("composer", "alt+shift+left", "word_back");
map("composer", "alt+right", "word_forward");
map("composer", "alt+shift+right", "word_forward");
map("composer", "ctrl+backspace", "delete_word");
map("composer", "ctrl+delete", "delete_next_word");
map("composer", "tab", "complete");
map("composer", "ctrl+v", "paste");
map("composer", "alt+v", "paste_image");
map("composer", "ctrl+z", "undo");
map("composer", "ctrl+shift+z", "redo");
map("composer", "ctrl+c", "copy");
map("composer", "alt+up", "restore_pending");
map("composer", "alt+enter", "upgrade_pending");
map("composer", "esc", "close");

map("list", "down", "next");
map("list", "up", "previous");
map("list", "j", "next");
map("list", "k", "previous");
map("list", "pagedown", "page_down");
map("list", "pageup", "page_up");
map("list", "enter", "confirm");
map("list", "esc", "close");

map("selector", "down", "next");
map("selector", "up", "previous");
map("selector", "pagedown", "page_down");
map("selector", "pageup", "page_up");
map("selector", "enter", "confirm");
map("selector", "esc", "close");
map("selector", "backspace", "backspace");

map("environment", "ctrl+n", "add");
map("environment", "delete", "remove");

map("tasks", "x", "cancel");

map("command", "enter", "confirm");
map("command", "esc", "cancel");
map("command", "backspace", "backspace");
map("command", "up", "previous");
map("command", "down", "next");
map("command", "pageup", "page_up");
map("command", "pagedown", "page_down");
map("command", "tab", "complete");
map("command", "backtab", "complete_previous");
map("command", "shift+tab", "complete_previous");
map("command", "shift+backtab", "complete_previous");

map("settings", "down", "next");
map("settings", "up", "previous");
map("settings", "j", "next");
map("settings", "k", "previous");
map("settings", "enter", "edit");
map("settings", "s", "save");
map("settings", "r", "refresh");
map("settings", "esc", "close");

map("models", "up", "previous");
map("models", "down", "next");
map("models", "pageup", "page_up");
map("models", "pagedown", "page_down");
map("models", "home", "first");
map("models", "end", "last");
map("models", "enter", "confirm");
map("models", "esc", "close");
map("models", "backspace", "backspace");
map("models", "ctrl+r", "refresh");

map("text", "enter", "confirm");
map("text", "esc", "cancel");
map("text", "backspace", "backspace");

map("oauth", "enter", "confirm");
map("oauth", "esc", "cancel");
map("oauth", "backspace", "backspace");

map("document", "up", "scroll_up");
map("document", "down", "scroll_down");
map("document", "pageup", "page_up");
map("document", "pagedown", "page_down");
map("document", "c", "copy");
map("document", "esc", "close");

map("selection", "y", "copy");
map("selection", "ctrl+c", "copy");
map("selection", "ctrl+shift+c", "copy");
map("selection", "super+c", "copy");
map("selection", "esc", "close");

map("terminal", "esc", "escape");
map("terminal", "ctrl+shift+c", "copy");
map("terminal", "super+c", "copy");
"#;

const MACOS_KEYMAP: &str = r#"
map("global", "super+,", "settings");
map("main", "super+v", "paste");
map("composer", "super+v", "paste");
map("composer", "super+z", "undo");
map("composer", "super+shift+z", "redo");
"#;

const CONTROL: u8 = 1;
const ALT: u8 = 1 << 1;
const SHIFT: u8 = 1 << 2;
const SUPER: u8 = 1 << 3;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyDisplayStyle {
    #[default]
    Auto,
    Macos,
    Text,
}

impl KeyDisplayStyle {
    fn resolve(self) -> Self {
        match self {
            Self::Auto if cfg!(target_os = "macos") => Self::Macos,
            Self::Auto => Self::Text,
            style => style,
        }
    }
}

impl FromStr for KeyDisplayStyle {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "macos" | "mac" => Ok(Self::Macos),
            "text" => Ok(Self::Text),
            _ => bail!("key display style must be auto, macos, or text"),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct KeyStroke {
    modifiers: u8,
    code: String,
}

impl KeyStroke {
    pub fn from_event(key: KeyEvent) -> Option<Self> {
        let mut modifiers = 0;
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            modifiers |= CONTROL;
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            modifiers |= ALT;
        }
        if key.modifiers.contains(KeyModifiers::SHIFT)
            && !matches!(key.code, KeyCode::Char(character) if !character.is_ascii_alphabetic())
        {
            modifiers |= SHIFT;
        }
        if key.modifiers.contains(KeyModifiers::SUPER) {
            modifiers |= SUPER;
        }
        let code = match key.code {
            KeyCode::Backspace => "backspace".to_string(),
            KeyCode::Enter => "enter".to_string(),
            KeyCode::Left => "left".to_string(),
            KeyCode::Right => "right".to_string(),
            KeyCode::Up => "up".to_string(),
            KeyCode::Down => "down".to_string(),
            KeyCode::Home => "home".to_string(),
            KeyCode::End => "end".to_string(),
            KeyCode::PageUp => "pageup".to_string(),
            KeyCode::PageDown => "pagedown".to_string(),
            KeyCode::Tab => "tab".to_string(),
            KeyCode::BackTab => "backtab".to_string(),
            KeyCode::Delete => "delete".to_string(),
            KeyCode::Insert => "insert".to_string(),
            KeyCode::F(number) => format!("f{number}"),
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(character) => {
                canonical_code(&character.to_lowercase().collect::<String>())
            }
            KeyCode::Null => "null".to_string(),
            KeyCode::Esc => "esc".to_string(),
            KeyCode::CapsLock => "capslock".to_string(),
            KeyCode::ScrollLock => "scrolllock".to_string(),
            KeyCode::NumLock => "numlock".to_string(),
            KeyCode::PrintScreen => "printscreen".to_string(),
            KeyCode::Pause => "pause".to_string(),
            KeyCode::Menu => "menu".to_string(),
            KeyCode::KeypadBegin => "keypadbegin".to_string(),
            KeyCode::Media(_) | KeyCode::Modifier(_) => return None,
        };
        Some(Self { modifiers, code })
    }

    pub fn canonical(&self) -> String {
        let mut parts = Vec::new();
        if self.modifiers & CONTROL != 0 {
            parts.push("ctrl");
        }
        if self.modifiers & ALT != 0 {
            parts.push("alt");
        }
        if self.modifiers & SHIFT != 0 {
            parts.push("shift");
        }
        if self.modifiers & SUPER != 0 {
            parts.push("super");
        }
        parts.push(&self.code);
        parts.join("+")
    }

    fn parse(value: &str) -> Result<Self> {
        let normalized = value.trim().to_ascii_lowercase();
        let mut remaining = normalized.as_str();
        let mut modifiers = 0;
        while let Some((prefix, rest)) = remaining.split_once('+') {
            let modifier = match prefix {
                "ctrl" | "control" => CONTROL,
                "alt" | "option" => ALT,
                "shift" => SHIFT,
                "super" | "cmd" | "command" => SUPER,
                _ => break,
            };
            modifiers |= modifier;
            remaining = rest;
        }
        let code = canonical_code(remaining);
        if !valid_code(&code) {
            bail!("invalid key {value:?}");
        }
        Ok(Self { modifiers, code })
    }

    fn display(&self, style: KeyDisplayStyle) -> String {
        match style {
            KeyDisplayStyle::Macos => self.display_macos(),
            KeyDisplayStyle::Text | KeyDisplayStyle::Auto => self.display_text(),
        }
    }

    fn display_modifiers(&self, style: KeyDisplayStyle) -> String {
        match style {
            KeyDisplayStyle::Macos => {
                let mut output = String::new();
                if self.modifiers & CONTROL != 0 {
                    output.push('⌃');
                }
                if self.modifiers & ALT != 0 {
                    output.push('⌥');
                }
                if self.modifiers & SHIFT != 0 {
                    output.push('⇧');
                }
                if self.modifiers & SUPER != 0 {
                    output.push('⌘');
                }
                output
            }
            KeyDisplayStyle::Text | KeyDisplayStyle::Auto => {
                let mut parts = Vec::new();
                if self.modifiers & CONTROL != 0 {
                    parts.push("Ctrl");
                }
                if self.modifiers & ALT != 0 {
                    parts.push("Alt");
                }
                if self.modifiers & SHIFT != 0 {
                    parts.push("Shift");
                }
                if self.modifiers & SUPER != 0 {
                    parts.push("Super");
                }
                parts.join("+")
            }
        }
    }

    fn display_text(&self) -> String {
        let modifiers = self.display_modifiers(KeyDisplayStyle::Text);
        let code = display_code(&self.code, false);
        if modifiers.is_empty() {
            code
        } else {
            format!("{modifiers}+{code}")
        }
    }

    fn display_macos(&self) -> String {
        let mut output = self.display_modifiers(KeyDisplayStyle::Macos);
        output.push_str(&display_code(&self.code, true));
        output
    }
}

#[derive(Clone)]
pub struct Keymap {
    bindings: Arc<Mutex<BTreeMap<(String, KeyStroke), String>>>,
    display_style: KeyDisplayStyle,
}

impl Default for Keymap {
    fn default() -> Self {
        Self {
            bindings: Arc::default(),
            display_style: KeyDisplayStyle::Text,
        }
    }
}

impl Keymap {
    pub async fn load(project: Option<&Path>, display_style: KeyDisplayStyle) -> Result<Self> {
        let keymap = Self::with_display_style(display_style)?;
        let global = config_directory()?.join("keymap.rhai");
        keymap.evaluate_file(&global).await?;
        if let Some(project) = project {
            keymap
                .evaluate_file(&project.join(".uri-agent/keymap.rhai"))
                .await?;
        }
        Ok(keymap)
    }

    #[cfg(test)]
    pub(crate) fn with_defaults() -> Result<Self> {
        Self::with_display_style(KeyDisplayStyle::Text)
    }

    pub(crate) fn with_display_style(display_style: KeyDisplayStyle) -> Result<Self> {
        let display_style = display_style.resolve();
        let keymap = Self {
            display_style,
            ..Self::default()
        };
        keymap.evaluate(DEFAULT_KEYMAP, "built-in keymap")?;
        if display_style == KeyDisplayStyle::Macos {
            keymap.evaluate(MACOS_KEYMAP, "built-in macOS keymap")?;
        }
        Ok(keymap)
    }

    pub fn action(&self, mode: &str, key: &str) -> Option<String> {
        self.action_chain(&[mode], key)
    }

    pub fn action_chain(&self, modes: &[&str], key: &str) -> Option<String> {
        let key = KeyStroke::parse(key).ok()?;
        let bindings = self.bindings.lock().unwrap();
        modes
            .iter()
            .find_map(|mode| bindings.get(&(mode.to_string(), key.clone())))
            .or_else(|| bindings.get(&("global".to_string(), key)))
            .cloned()
    }

    pub fn key_for(&self, mode: &str, action: &str) -> Option<String> {
        let bindings = self.bindings.lock().unwrap();
        bindings
            .iter()
            .filter(|((binding_mode, _), binding_action)| {
                binding_mode == mode && binding_action.as_str() == action
            })
            .min_by_key(|((_, key), _)| key_preference(key, self.display_style))
            .or_else(|| {
                bindings
                    .iter()
                    .filter(|((binding_mode, _), binding_action)| {
                        binding_mode == "global" && binding_action.as_str() == action
                    })
                    .min_by_key(|((_, key), _)| key_preference(key, self.display_style))
            })
            .map(|((_, key), _)| key.canonical())
    }

    pub fn key_hint(&self, mode: &str, action: &str) -> Option<String> {
        let bindings = self.bindings.lock().unwrap();
        bindings
            .iter()
            .filter(|((binding_mode, _), binding_action)| {
                binding_mode == mode && binding_action.as_str() == action
            })
            .min_by_key(|((_, key), _)| key_preference(key, self.display_style))
            .or_else(|| {
                bindings
                    .iter()
                    .filter(|((binding_mode, _), binding_action)| {
                        binding_mode == "global" && binding_action.as_str() == action
                    })
                    .min_by_key(|((_, key), _)| key_preference(key, self.display_style))
            })
            .map(|((_, key), _)| key.display(self.display_style))
    }

    pub fn display_key(&self, key: &str) -> Option<String> {
        KeyStroke::parse(key)
            .ok()
            .map(|key| key.display(self.display_style))
    }

    pub fn modifier_hint(&self, modifier: &str) -> Option<String> {
        KeyStroke::parse(&format!("{modifier}+x"))
            .ok()
            .map(|key| key.display_modifiers(self.display_style))
            .filter(|hint| !hint.is_empty())
    }

    pub fn display_bindings_for(&self, mode: &str) -> Vec<(String, String)> {
        let bindings = self.bindings.lock().unwrap();
        let mut bindings = bindings
            .iter()
            .filter(|((binding_mode, _), _)| binding_mode == mode)
            .map(|((_, key), action)| (key.clone(), action.clone()))
            .collect::<Vec<_>>();
        bindings.sort_by_key(|(key, _)| key_preference(key, self.display_style));
        bindings
            .into_iter()
            .map(|(key, action)| (key.display(self.display_style), action))
            .collect()
    }

    pub fn paths(project: Option<&Path>) -> Result<Vec<PathBuf>> {
        let mut paths = vec![config_directory()?.join("keymap.rhai")];
        if let Some(project) = project {
            paths.push(project.join(".uri-agent/keymap.rhai"));
        }
        Ok(paths)
    }

    async fn evaluate_file(&self, path: &Path) -> Result<()> {
        let source = match fs::read_to_string(path).await {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        self.evaluate(&source, &path.display().to_string())
    }

    fn evaluate(&self, source: &str, label: &str) -> Result<()> {
        let mut engine = Engine::new();
        engine.set_max_operations(100_000);
        let invalid_keys = Arc::new(Mutex::new(Vec::new()));
        let mapped = self.bindings.clone();
        let map_errors = invalid_keys.clone();
        engine.register_fn(
            "map",
            move |mode: ImmutableString, key: ImmutableString, action: ImmutableString| {
                match KeyStroke::parse(&key) {
                    Ok(key) => {
                        mapped
                            .lock()
                            .unwrap()
                            .insert((mode.to_string(), key), action.to_string());
                    }
                    Err(error) => map_errors.lock().unwrap().push(error.to_string()),
                }
            },
        );
        let unmapped = self.bindings.clone();
        let unmap_errors = invalid_keys.clone();
        engine.register_fn(
            "unmap",
            move |mode: ImmutableString, key: ImmutableString| match KeyStroke::parse(&key) {
                Ok(key) => {
                    unmapped.lock().unwrap().remove(&(mode.to_string(), key));
                }
                Err(error) => unmap_errors.lock().unwrap().push(error.to_string()),
            },
        );
        engine
            .eval::<()>(source)
            .with_context(|| format!("cannot evaluate {label}"))?;
        if let Some(error) = invalid_keys.lock().unwrap().first() {
            bail!("cannot evaluate {label}: {error}");
        }
        Ok(())
    }
}

fn canonical_code(code: &str) -> String {
    match code {
        "：" => ":".to_string(),
        "？" => "?".to_string(),
        other => other.to_string(),
    }
}

fn valid_code(code: &str) -> bool {
    const NAMED_KEYS: [&str; 24] = [
        "backspace",
        "enter",
        "left",
        "right",
        "up",
        "down",
        "home",
        "end",
        "pageup",
        "pagedown",
        "tab",
        "backtab",
        "delete",
        "insert",
        "space",
        "null",
        "esc",
        "capslock",
        "scrolllock",
        "numlock",
        "printscreen",
        "pause",
        "menu",
        "keypadbegin",
    ];
    code.chars().count() == 1
        || NAMED_KEYS.contains(&code)
        || code
            .strip_prefix('f')
            .and_then(|number| number.parse::<u8>().ok())
            .is_some_and(|number| number > 0)
}

fn display_code(code: &str, macos: bool) -> String {
    if macos {
        match code {
            "left" => return "←".to_string(),
            "right" => return "→".to_string(),
            "up" => return "↑".to_string(),
            "down" => return "↓".to_string(),
            "enter" => return "↩".to_string(),
            "backspace" => return "⌫".to_string(),
            "delete" => return "⌦".to_string(),
            "tab" => return "⇥".to_string(),
            "backtab" => return "⇤".to_string(),
            "pageup" => return "⇞".to_string(),
            "pagedown" => return "⇟".to_string(),
            "home" => return "↖".to_string(),
            "end" => return "↘".to_string(),
            _ => {}
        }
    }
    match code {
        "esc" => "Esc".to_string(),
        "pageup" => "PageUp".to_string(),
        "pagedown" => "PageDown".to_string(),
        "backtab" => "BackTab".to_string(),
        "keypadbegin" => "KeypadBegin".to_string(),
        code if code.starts_with('f')
            && code[1..]
                .chars()
                .all(|character| character.is_ascii_digit()) =>
        {
            code.to_ascii_uppercase()
        }
        code if code.chars().count() == 1 => code.to_uppercase(),
        code => {
            let mut characters = code.chars();
            characters
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
                .unwrap_or_default()
        }
    }
}

fn key_preference(key: &KeyStroke, style: KeyDisplayStyle) -> (usize, usize, String) {
    const CONVENTIONAL_KEYS: [&str; 15] = [
        "up",
        "down",
        "left",
        "right",
        "enter",
        "esc",
        "pageup",
        "pagedown",
        "home",
        "end",
        "backspace",
        "space",
        "tab",
        "backtab",
        "delete",
    ];
    (
        CONVENTIONAL_KEYS
            .iter()
            .position(|candidate| candidate == &key.code)
            .unwrap_or(CONVENTIONAL_KEYS.len()),
        modifier_preference(key.modifiers, style),
        key.canonical(),
    )
}

fn modifier_preference(modifiers: u8, style: KeyDisplayStyle) -> usize {
    let ordered = if style == KeyDisplayStyle::Macos {
        [
            0,
            SHIFT,
            SUPER,
            SUPER | SHIFT,
            CONTROL,
            CONTROL | SHIFT,
            ALT,
            ALT | SHIFT,
        ]
    } else if cfg!(windows) {
        // Windows consoles do not deliver Shift+Enter as a key press, so
        // advertise an action's Ctrl-based binding over the Shift-based one.
        [
            0,
            CONTROL,
            SHIFT,
            CONTROL | SHIFT,
            ALT,
            ALT | SHIFT,
            SUPER,
            SUPER | SHIFT,
        ]
    } else {
        [
            0,
            SHIFT,
            CONTROL,
            CONTROL | SHIFT,
            ALT,
            ALT | SHIFT,
            SUPER,
            SUPER | SHIFT,
        ]
    };
    ordered
        .iter()
        .position(|candidate| *candidate == modifiers)
        .unwrap_or(ordered.len() + modifiers.count_ones() as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rhai_keymaps_override_and_unmap_defaults() {
        let keymap = Keymap::default();
        keymap.evaluate(DEFAULT_KEYMAP, "defaults").unwrap();
        keymap
            .evaluate(
                r#"
                map("main", "x", "copy");
                unmap("main", "j");
                "#,
                "test",
            )
            .unwrap();
        assert_eq!(keymap.action("main", "x").as_deref(), Some("copy"));
        assert_eq!(keymap.action("main", "j"), None);
        assert_eq!(keymap.action("main", "f1").as_deref(), Some("help"));
    }

    #[test]
    fn defaults_keep_conventional_navigation_and_discoverable_commands() {
        let keymap = Keymap::default();
        keymap.evaluate(DEFAULT_KEYMAP, "defaults").unwrap();

        assert_eq!(keymap.action("main", "down").as_deref(), Some("next"));
        assert_eq!(keymap.action("main", "up").as_deref(), Some("previous"));
        assert_eq!(keymap.key_for("main", "previous").as_deref(), Some("up"));
        assert_eq!(
            keymap.action("main", "ctrl+down").as_deref(),
            Some("scroll_down")
        );
        assert_eq!(
            keymap.action("main", "ctrl+up").as_deref(),
            Some("scroll_up")
        );
        assert_eq!(keymap.action("main", ":").as_deref(), Some("command"));
        assert_eq!(keymap.action("main", "：").as_deref(), Some("command"));
        assert_eq!(keymap.action("main", "?").as_deref(), Some("help"));
        assert_eq!(keymap.action("main", "？").as_deref(), Some("help"));
        assert_eq!(keymap.action("main", "space").as_deref(), Some("compose"));
        assert_eq!(keymap.action("main", "i"), None);
        assert_eq!(keymap.action("main", "@").as_deref(), Some("reference"));
        assert_eq!(keymap.action("main", "ctrl+v").as_deref(), Some("paste"));
        assert_eq!(
            keymap.action("main", "alt+v").as_deref(),
            Some("paste_image")
        );
        assert_eq!(keymap.action("main", "alt+backspace"), None);
        assert_eq!(keymap.action("main", "o").as_deref(), Some("open"));
        assert_eq!(keymap.action("document", "c").as_deref(), Some("copy"));
        assert_eq!(keymap.action("document", "esc").as_deref(), Some("close"));
        assert_eq!(
            keymap.action("command", "pagedown").as_deref(),
            Some("page_down")
        );
        assert_eq!(
            keymap.action("selector", "pageup").as_deref(),
            Some("page_up")
        );
        assert_eq!(keymap.action("main", "q"), None);
        assert_eq!(keymap.action("main", "ctrl+c"), None);
        assert_eq!(keymap.action("composer", "ctrl+c").as_deref(), Some("copy"));
        assert_eq!(
            keymap.action("selection", "ctrl+c").as_deref(),
            Some("copy")
        );
        assert_eq!(
            keymap.action("selection", "super+c").as_deref(),
            Some("copy")
        );
        assert_eq!(keymap.action("main", "super+c").as_deref(), Some("copy"));
        assert_eq!(
            keymap.action("main", "r").as_deref(),
            Some("jump_reasoning")
        );
        assert_eq!(
            keymap.action("composer", "enter").as_deref(),
            Some("submit")
        );
        assert_eq!(
            keymap.action("composer", "shift+enter").as_deref(),
            Some("newline")
        );
        assert_eq!(
            keymap.action("composer", "up").as_deref(),
            Some("cursor_up")
        );
        assert_eq!(
            keymap.action("composer", "down").as_deref(),
            Some("cursor_down")
        );
        assert_eq!(
            keymap.action("composer", "ctrl+home").as_deref(),
            Some("first")
        );
        assert_eq!(
            keymap.action("composer", "ctrl+end").as_deref(),
            Some("last")
        );
        assert_eq!(
            keymap.action("composer", "ctrl+backspace").as_deref(),
            Some("delete_word")
        );
        assert_eq!(
            keymap.action("composer", "ctrl+delete").as_deref(),
            Some("delete_next_word")
        );
        assert_eq!(
            keymap.action("composer", "tab").as_deref(),
            Some("complete")
        );
        assert_eq!(
            keymap.action("composer", "ctrl+v").as_deref(),
            Some("paste")
        );
        assert_eq!(
            keymap.action("composer", "alt+v").as_deref(),
            Some("paste_image")
        );
        assert_eq!(keymap.action("composer", "alt+backspace"), None);
        assert_eq!(keymap.action("composer", "ctrl+z").as_deref(), Some("undo"));
        assert_eq!(
            keymap.action("composer", "ctrl+shift+z").as_deref(),
            Some("redo")
        );
        assert_eq!(keymap.action("composer", "esc").as_deref(), Some("close"));
        assert_eq!(
            keymap.action("command", "enter").as_deref(),
            Some("confirm")
        );
        assert_eq!(
            keymap.action("command", "backspace").as_deref(),
            Some("backspace")
        );
        assert_eq!(keymap.action("command", "tab").as_deref(), Some("complete"));
        assert_eq!(
            keymap.action("command", "backtab").as_deref(),
            Some("complete_previous")
        );
        assert_eq!(
            keymap.action("command", "shift+backtab").as_deref(),
            Some("complete_previous")
        );
        assert_eq!(keymap.action("global", "f3").as_deref(), Some("model"));
        assert_eq!(keymap.action("global", "f4").as_deref(), Some("status"));
        assert_eq!(
            keymap.action("global", "esc").as_deref(),
            Some("interrupt_on_double_press")
        );
        assert_eq!(keymap.action("models", "down").as_deref(), Some("next"));
        assert_eq!(keymap.action("selector", "down").as_deref(), Some("next"));
        assert_eq!(keymap.action("selector", "k"), None);
        assert_eq!(keymap.action("terminal", "esc").as_deref(), Some("escape"));
        assert_eq!(
            keymap.action("terminal", "super+c").as_deref(),
            Some("copy")
        );
    }

    #[test]
    fn key_strokes_normalize_events_and_configuration_aliases() {
        let event = KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::SUPER | KeyModifiers::SHIFT,
        );
        assert_eq!(
            KeyStroke::from_event(event).unwrap().canonical(),
            "shift+super+c"
        );
        assert_eq!(
            KeyStroke::parse("Command+Shift+C").unwrap().canonical(),
            "shift+super+c"
        );
        assert_eq!(
            KeyStroke::parse("Option+Left").unwrap().canonical(),
            "alt+left"
        );
        assert_eq!(KeyStroke::parse("？").unwrap().canonical(), "?");
    }

    #[test]
    fn invalid_rhai_keys_fail_during_keymap_loading() {
        let keymap = Keymap::default();
        let error = keymap
            .evaluate(r#"map("main", "ctrl+not-a-key", "copy");"#, "test keymap")
            .unwrap_err();
        assert!(error.to_string().contains("invalid key"));
    }

    #[test]
    fn text_and_macos_styles_format_the_effective_binding() {
        let text = Keymap::with_display_style(KeyDisplayStyle::Text).unwrap();
        let newline = if cfg!(windows) {
            "Ctrl+Enter"
        } else {
            "Shift+Enter"
        };
        assert_eq!(
            text.key_hint("composer", "newline").as_deref(),
            Some(newline)
        );
        assert_eq!(
            text.key_hint("composer", "paste_image").as_deref(),
            Some("Alt+V")
        );
        assert_eq!(
            text.key_hint("global", "copy").as_deref(),
            Some("Ctrl+Shift+C")
        );

        let macos = Keymap::with_display_style(KeyDisplayStyle::Macos).unwrap();
        assert_eq!(macos.key_hint("main", "previous").as_deref(), Some("↑"));
        assert_eq!(macos.key_hint("composer", "newline").as_deref(), Some("⇧↩"));
        assert_eq!(
            macos.key_hint("composer", "paste_image").as_deref(),
            Some("⌥V")
        );
        assert_eq!(macos.key_hint("global", "copy").as_deref(), Some("⌘C"));
        assert_eq!(macos.key_hint("main", "paste").as_deref(), Some("⌘V"));
        assert_eq!(macos.modifier_hint("shift").as_deref(), Some("⇧"));
        assert_eq!(macos.action("composer", "cmd+z").as_deref(), Some("undo"));
    }

    #[test]
    fn displayed_hints_follow_user_overrides_and_unmaps() {
        let keymap = Keymap::with_display_style(KeyDisplayStyle::Macos).unwrap();
        keymap
            .evaluate(
                r#"
                unmap("composer", "shift+enter");
                unmap("composer", "ctrl+enter");
                unmap("composer", "ctrl+j");
                map("composer", "cmd+k", "newline");
                "#,
                "test keymap",
            )
            .unwrap();
        assert_eq!(
            keymap.key_hint("composer", "newline").as_deref(),
            Some("⌘K")
        );
        assert_eq!(
            keymap.action("composer", "super+k").as_deref(),
            Some("newline")
        );
    }
}
