use std::io;
use std::path::{Component, Path, PathBuf};
use tokio::fs;

const MAX_SYMLINK_HOPS: usize = 40;

/// Resolve the destination for an atomic replacement without replacing a
/// symbolic link that names the file being written.
///
/// `canonicalize` handles complete chains. A dangling chain needs a manual
/// walk so its final target can be created while every existing link remains
/// intact.
pub(crate) async fn resolve_write_path(path: &Path) -> io::Result<PathBuf> {
    let path = absolute_path(path)?;
    match fs::canonicalize(&path).await {
        Ok(resolved) => return Ok(resolved),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    match fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => {}
        Ok(_) => return Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(path),
        Err(error) => return Err(error),
    }

    let mut current = path.clone();
    for _ in 0..MAX_SYMLINK_HOPS {
        let target = match fs::read_link(&current).await {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(error) => return Err(error),
        };
        let resolved = resolve_link_target(&current, &target).await?;
        match fs::symlink_metadata(&resolved).await {
            Ok(metadata) if metadata.file_type().is_symlink() => current = resolved,
            Ok(_) => return Ok(resolved),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(resolved),
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "symbolic link chain for {} exceeds {MAX_SYMLINK_HOPS} hops",
            path.display()
        ),
    ))
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

async fn resolve_link_target(link: &Path, target: &Path) -> io::Result<PathBuf> {
    let mut resolved = if target.is_absolute() {
        PathBuf::new()
    } else {
        let parent = link.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("symbolic link has no parent: {}", link.display()),
            )
        })?;
        fs::canonicalize(parent).await?
    };
    let mut unresolved = false;

    for component in target.components() {
        match component {
            Component::Prefix(prefix) => {
                if !resolved.as_os_str().is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid symbolic link target: {}", target.display()),
                    ));
                }
                resolved.push(prefix.as_os_str());
            }
            Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => require_directory(&resolved, unresolved).await?,
            Component::ParentDir => {
                require_directory(&resolved, unresolved).await?;
                resolved.pop();
            }
            Component::Normal(segment) if unresolved => resolved.push(segment),
            Component::Normal(segment) => {
                let candidate = resolved.join(segment);
                match fs::canonicalize(&candidate).await {
                    Ok(canonical) => resolved = canonical,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        resolved = candidate;
                        unresolved = true;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }

    Ok(resolved)
}

async fn require_directory(path: &Path, unresolved: bool) -> io::Result<()> {
    if unresolved {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "cannot traverse an unresolved symbolic link target",
        ));
    }
    match fs::metadata(path).await {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!(
                "symbolic link target is not a directory: {}",
                path.display()
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("symbolic link target disappeared: {}", path.display()),
        )),
        Err(error) => Err(error),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[tokio::test]
    async fn resolves_dangling_multihop_chain_to_final_target() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("settings.json");
        let second = root.path().join("managed-link");
        let target = root.path().join("managed/settings.json");
        std::fs::create_dir(root.path().join("managed")).unwrap();
        symlink("managed/settings.json", &second).unwrap();
        symlink("managed-link", &first).unwrap();

        assert_eq!(resolve_write_path(&first).await.unwrap(), target);
    }

    #[tokio::test]
    async fn resolves_parent_components_after_directory_links_physically() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("managed/nested");
        std::fs::create_dir_all(&nested).unwrap();
        symlink(&nested, root.path().join("directory-link")).unwrap();
        let config = root.path().join("settings.json");
        symlink("directory-link/../settings.json", &config).unwrap();

        assert_eq!(
            resolve_write_path(&config).await.unwrap(),
            root.path().join("managed/settings.json")
        );
    }

    #[tokio::test]
    async fn rejects_parent_traversal_after_a_missing_component() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("settings.json");
        symlink("missing/../unrelated.json", &config).unwrap();

        let error = resolve_write_path(&config).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
        assert!(std::fs::symlink_metadata(config).unwrap().is_symlink());
    }

    #[tokio::test]
    async fn rejects_symlink_chains_beyond_the_hop_limit() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..=MAX_SYMLINK_HOPS {
            let target = if index == MAX_SYMLINK_HOPS {
                "missing".to_string()
            } else {
                format!("link-{}", index + 1)
            };
            symlink(target, root.path().join(format!("link-{index}"))).unwrap();
        }

        assert!(
            resolve_write_path(&root.path().join("link-0"))
                .await
                .is_err()
        );
    }
}
