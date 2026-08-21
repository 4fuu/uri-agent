use crate::config::config_directory;
use anyhow::{Context, Result};
use rhai::{Engine, ImmutableString};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::fs;

const DEFAULT_KEYMAP: &str = r#"
map("global", "ctrl+c", "quit");
map("global", "f1", "help");
map("global", "f2", "settings");
map("global", "ctrl+,", "settings");
map("global", "ctrl+p", "protocols");
map("global", "ctrl+t", "tasks");
map("global", "ctrl+shift+c", "copy");

map("browse", "space", "palette");
map("browse", ":", "command");
map("browse", "/", "finder");
map("browse", "i", "insert");
map("browse", "j", "next");
map("browse", "down", "next");
map("browse", "k", "previous");
map("browse", "up", "previous");
map("browse", "ctrl+d", "page_down");
map("browse", "pagedown", "page_down");
map("browse", "ctrl+u", "page_up");
map("browse", "pageup", "page_up");
map("browse", "g", "first");
map("browse", "home", "first");
map("browse", "shift+g", "last");
map("browse", "end", "last");
map("browse", "enter", "detail");
map("browse", "o", "detail");
map("browse", "e", "editor");
map("browse", "y", "copy");
map("browse", "q", "quit");

map("insert", "esc", "browse");
map("insert", "enter", "send");
map("insert", "shift+enter", "newline");
map("insert", "ctrl+e", "editor");
map("insert", "ctrl+d", "quit_empty");

map("detail", "j", "scroll_down");
map("detail", "down", "scroll_down");
map("detail", "k", "scroll_up");
map("detail", "up", "scroll_up");
map("detail", "pagedown", "page_down");
map("detail", "pageup", "page_up");
map("detail", "e", "editor");
map("detail", "esc", "close");

map("list", "j", "next");
map("list", "down", "next");
map("list", "k", "previous");
map("list", "up", "previous");
map("list", "pagedown", "page_down");
map("list", "pageup", "page_up");
map("list", "esc", "close");

map("tasks", "e", "editor");
map("tasks", "x", "cancel");

map("palette", "j", "next");
map("palette", "down", "next");
map("palette", "k", "previous");
map("palette", "up", "previous");
map("palette", "enter", "confirm");
map("palette", "esc", "close");

map("command", "enter", "confirm");
map("command", "esc", "cancel");
map("command", "backspace", "backspace");

map("settings", "j", "next");
map("settings", "down", "next");
map("settings", "k", "previous");
map("settings", "up", "previous");
map("settings", "h", "left");
map("settings", "left", "left");
map("settings", "l", "right");
map("settings", "right", "right");
map("settings", "enter", "edit");
map("settings", "s", "save");
map("settings", "r", "refresh");
map("settings", "x", "clear");
map("settings", "esc", "close");

map("text", "enter", "confirm");
map("text", "esc", "cancel");
map("text", "backspace", "backspace");

map("selection", "y", "copy");
map("selection", "ctrl+shift+c", "copy");
map("selection", "esc", "close");

map("terminal", "esc", "escape");
map("terminal", "ctrl+shift+c", "copy");
"#;

#[derive(Clone, Default)]
pub struct Keymap {
    bindings: Arc<Mutex<BTreeMap<(String, String), String>>>,
}

impl Keymap {
    pub async fn load(project: Option<&Path>) -> Result<Self> {
        let keymap = Self::default();
        keymap.evaluate(DEFAULT_KEYMAP, "built-in keymap")?;
        let global = config_directory()?.join("keymap.rhai");
        keymap.evaluate_file(&global).await?;
        if let Some(project) = project {
            keymap
                .evaluate_file(&project.join(".uri-agent/keymap.rhai"))
                .await?;
        }
        Ok(keymap)
    }

    pub fn action(&self, mode: &str, key: &str) -> Option<String> {
        self.action_chain(&[mode], key)
    }

    pub fn action_chain(&self, modes: &[&str], key: &str) -> Option<String> {
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
                mapped
                    .lock()
                    .unwrap()
                    .insert((mode.to_string(), key.to_string()), action.to_string());
            },
        );
        let unmapped = self.bindings.clone();
        engine.register_fn(
            "unmap",
            move |mode: ImmutableString, key: ImmutableString| {
                unmapped
                    .lock()
                    .unwrap()
                    .remove(&(mode.to_string(), key.to_string()));
            },
        );
        engine
            .eval::<()>(source)
            .with_context(|| format!("cannot evaluate {label}"))
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
                map("browse", "x", "finder");
                unmap("browse", "j");
                "#,
                "test",
            )
            .unwrap();
        assert_eq!(keymap.action("browse", "x").as_deref(), Some("finder"));
        assert_eq!(keymap.action("browse", "j"), None);
        assert_eq!(keymap.action("browse", "f1").as_deref(), Some("help"));
    }

    #[test]
    fn defaults_keep_conventional_navigation_and_discoverable_commands() {
        let keymap = Keymap::default();
        keymap.evaluate(DEFAULT_KEYMAP, "defaults").unwrap();

        assert_eq!(keymap.action("browse", "down").as_deref(), Some("next"));
        assert_eq!(keymap.action("browse", "up").as_deref(), Some("previous"));
        assert_eq!(keymap.key_for("browse", "previous").as_deref(), Some("up"));
        assert_eq!(keymap.action("browse", "space").as_deref(), Some("palette"));
        assert_eq!(keymap.action("browse", ":").as_deref(), Some("command"));
        assert_eq!(
            keymap.action("palette", "enter").as_deref(),
            Some("confirm")
        );
    }
}
