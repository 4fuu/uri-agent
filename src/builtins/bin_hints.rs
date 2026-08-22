use crate::plugin::{Plugin, PluginHost};
use anyhow::Result;
use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;

const CANDIDATES: &[&str] = &[
    "rg",
    "fd",
    "fdfind",
    "sd",
    "bat",
    "batcat",
    "eza",
    "exa",
    "lsd",
    "delta",
    "jq",
    "yq",
    "fzf",
    "xh",
    "hyperfine",
    "dust",
    "duf",
    "procs",
    "btm",
    "zoxide",
    "doggo",
    "gping",
    "hexyl",
    "choose",
    "sad",
    "ast-grep",
    "broot",
    "tokei",
    "watchexec",
    "glow",
];

pub(super) struct BinHintsPlugin;

impl Plugin for BinHintsPlugin {
    fn system_prompt_fragment(&self) -> Result<Option<String>> {
        let path = env::var_os("PATH");
        let pathext = env::var_os("PATHEXT");
        Ok(prompt_fragment(&detect_binaries(
            path.as_deref(),
            pathext.as_deref(),
        )))
    }

    fn register(&self, _host: &mut PluginHost<'_>) -> Result<()> {
        Ok(())
    }
}

fn detect_binaries(path: Option<&OsStr>, pathext: Option<&OsStr>) -> Vec<&'static str> {
    let wanted = CANDIDATES.iter().copied().collect::<HashSet<_>>();
    let mut found = HashSet::new();

    #[cfg(windows)]
    let extensions = pathext
        .unwrap_or_else(|| OsStr::new(".COM;.EXE;.BAT;.CMD"))
        .to_string_lossy()
        .to_lowercase()
        .split(';')
        .map(str::to_string)
        .collect::<Vec<_>>();
    #[cfg(not(windows))]
    let extensions = {
        let _ = pathext;
        Vec::new()
    };

    #[cfg(windows)]
    let directories = path
        .unwrap_or_else(|| OsStr::new(""))
        .to_string_lossy()
        .split(';')
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    #[cfg(not(windows))]
    let directories = env::split_paths(path.unwrap_or_else(|| OsStr::new(""))).collect::<Vec<_>>();

    for directory in directories {
        if directory.as_os_str().is_empty() {
            continue;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };

        for entry in entries.flatten() {
            let Some(name) = candidate_name(&entry.file_name(), &extensions) else {
                continue;
            };
            if !wanted.contains(name.as_str()) || found.contains(name.as_str()) {
                continue;
            }
            if !is_executable_file(entry.path()) {
                continue;
            }
            found.insert(name);
        }
    }

    CANDIDATES
        .iter()
        .copied()
        .filter(|name| found.contains(*name))
        .collect()
}

#[cfg(not(windows))]
fn candidate_name(file_name: &OsStr, _extensions: &[String]) -> Option<String> {
    Some(file_name.to_string_lossy().to_lowercase())
}

#[cfg(windows)]
fn candidate_name(file_name: &OsStr, extensions: &[String]) -> Option<String> {
    let file_name = file_name.to_string_lossy().to_lowercase();
    let dot = file_name.rfind('.')?;
    if dot == 0
        || !extensions
            .iter()
            .any(|extension| extension == &file_name[dot..])
    {
        return None;
    }
    Some(file_name[..dot].to_string())
}

#[cfg(unix)]
fn is_executable_file(path: PathBuf) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable_file(_path: PathBuf) -> bool {
    true
}

#[cfg(not(any(unix, windows)))]
fn is_executable_file(path: PathBuf) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn prompt_fragment(binaries: &[&str]) -> Option<String> {
    if binaries.is_empty() {
        return None;
    }
    let names = binaries
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(" / ");
    Some(format!(
        "These faster cross-platform tools are available: {names}. \
         Prefer them over their classical Unix equivalents."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_uses_the_original_order_and_wording() {
        assert_eq!(
            prompt_fragment(&["rg", "fdfind", "jq"]),
            Some(
                "These faster cross-platform tools are available: `rg` / `fdfind` / `jq`. \
                 Prefer them over their classical Unix equivalents."
                    .to_string()
            )
        );
        assert_eq!(prompt_fragment(&[]), None);
    }

    #[cfg(unix)]
    #[test]
    fn unix_detection_orders_deduplicates_and_requires_executable_files() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();

        fs::write(first.join("bat"), "").unwrap();
        fs::set_permissions(first.join("bat"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(second.join("bat"), "").unwrap();
        fs::set_permissions(second.join("bat"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(second.join("RG"), "").unwrap();
        fs::set_permissions(second.join("RG"), fs::Permissions::from_mode(0o100)).unwrap();

        fs::write(first.join("fd"), "").unwrap();
        fs::set_permissions(first.join("fd"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::create_dir(first.join("yq")).unwrap();
        let target = root.path().join("target");
        fs::write(&target, "").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, first.join("jq")).unwrap();

        let path =
            env::join_paths([root.path().join("missing"), PathBuf::new(), first, second]).unwrap();

        assert_eq!(detect_binaries(Some(&path), None), vec!["rg", "bat", "jq"]);
    }

    #[cfg(unix)]
    #[test]
    fn missing_or_empty_paths_contribute_no_binaries() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let path = env::join_paths([PathBuf::new(), missing]).unwrap();

        assert!(detect_binaries(Some(&path), None).is_empty());
        assert!(detect_binaries(None, None).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_detection_uses_pathext_without_file_metadata_checks() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("rg.EXE")).unwrap();
        fs::write(root.path().join("bat.CMD"), "").unwrap();
        let path = root.path().as_os_str();

        assert_eq!(detect_binaries(Some(path), None), vec!["rg", "bat"]);
        assert_eq!(
            detect_binaries(Some(path), Some(OsStr::new(".CMD"))),
            vec!["bat"]
        );
        assert!(detect_binaries(Some(path), Some(OsStr::new(""))).is_empty());
    }
}
