use crate::config::config_directory;
use anyhow::{Context, Result};
use rhai::{Engine, ImmutableString};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
map("main", "@", "paste_image");
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
map("composer", "alt+backspace", "remove_last_image");
map("composer", "ctrl+z", "undo");
map("composer", "ctrl+shift+z", "redo");
map("composer", "ctrl+c", "copy");
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
map("selector", "enter", "confirm");
map("selector", "esc", "close");
map("selector", "backspace", "backspace");

map("tasks", "x", "cancel");

map("command", "enter", "confirm");
map("command", "esc", "cancel");
map("command", "backspace", "backspace");
map("command", "up", "previous");
map("command", "down", "next");
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

#[derive(Clone, Default)]
pub struct Keymap {
    bindings: Arc<Mutex<BTreeMap<(String, String), String>>>,
}

impl Keymap {
    pub async fn load(project: Option<&Path>) -> Result<Self> {
        let keymap = Self::with_defaults()?;
        let global = config_directory()?.join("keymap.rhai");
        keymap.evaluate_file(&global).await?;
        if let Some(project) = project {
            keymap
                .evaluate_file(&project.join(".uri-agent/keymap.rhai"))
                .await?;
        }
        Ok(keymap)
    }

    pub(crate) fn with_defaults() -> Result<Self> {
        let keymap = Self::default();
        keymap.evaluate(DEFAULT_KEYMAP, "built-in keymap")?;
        Ok(keymap)
    }

    pub fn action(&self, mode: &str, key: &str) -> Option<String> {
        self.action_chain(&[mode], key)
    }

    pub fn action_chain(&self, modes: &[&str], key: &str) -> Option<String> {
        let key = canonical_key(key);
        let bindings = self.bindings.lock().unwrap();
        modes
            .iter()
            .find_map(|mode| bindings.get(&(mode.to_string(), key.to_string())))
            .or_else(|| bindings.get(&("global".to_string(), key.to_string())))
            .cloned()
    }

    pub fn key_for(&self, mode: &str, action: &str) -> Option<String> {
        let bindings = self.bindings.lock().unwrap();
        bindings
            .iter()
            .filter(|((binding_mode, _), binding_action)| {
                binding_mode == mode && binding_action.as_str() == action
            })
            .min_by_key(|((_, key), _)| key_preference(key))
            .or_else(|| {
                bindings
                    .iter()
                    .filter(|((binding_mode, _), binding_action)| {
                        binding_mode == "global" && binding_action.as_str() == action
                    })
                    .min_by_key(|((_, key), _)| key_preference(key))
            })
            .map(|((_, key), _)| key.clone())
    }

    pub fn bindings_for(&self, mode: &str) -> Vec<(String, String)> {
        let bindings = self.bindings.lock().unwrap();
        bindings
            .iter()
            .filter(|((binding_mode, _), _)| binding_mode == mode)
            .map(|((_, key), action)| (key.clone(), action.clone()))
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
        let mapped = self.bindings.clone();
        engine.register_fn(
            "map",
            move |mode: ImmutableString, key: ImmutableString, action: ImmutableString| {
                mapped.lock().unwrap().insert(
                    (mode.to_string(), canonical_key(&key).to_string()),
                    action.to_string(),
                );
            },
        );
        let unmapped = self.bindings.clone();
        engine.register_fn(
            "unmap",
            move |mode: ImmutableString, key: ImmutableString| {
                unmapped
                    .lock()
                    .unwrap()
                    .remove(&(mode.to_string(), canonical_key(&key).to_string()));
            },
        );
        engine
            .eval::<()>(source)
            .with_context(|| format!("cannot evaluate {label}"))
    }
}

pub(crate) fn canonical_key(key: &str) -> &str {
    match key {
        "：" => ":",
        "？" => "?",
        other => other,
    }
}

fn key_preference(key: &str) -> (usize, &str) {
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
            .position(|candidate| candidate == &key)
            .unwrap_or(CONVENTIONAL_KEYS.len()),
        key,
    )
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
        assert_eq!(keymap.action("main", "@").as_deref(), Some("paste_image"));
        assert_eq!(keymap.action("main", "o").as_deref(), Some("open"));
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
            keymap.action("composer", "alt+backspace").as_deref(),
            Some("remove_last_image")
        );
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
}
