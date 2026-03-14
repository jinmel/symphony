use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PathSafetyError {
    #[error("path_canonicalize_failed: {path}: {reason}")]
    CanonicalizeFailed { path: PathBuf, reason: String },
    #[error("invalid_workspace_cwd: workspace_root")]
    WorkspaceRoot,
    #[error("invalid_workspace_cwd: outside_workspace_root: workspace={workspace} root={root}")]
    OutsideWorkspaceRoot { workspace: String, root: String },
    #[error("invalid_workspace_cwd: symlink_escape: workspace={workspace} root={root}")]
    SymlinkEscape { workspace: String, root: String },
}

pub fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf, PathSafetyError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| PathSafetyError::CanonicalizeFailed {
                path: path.to_path_buf(),
                reason: error.to_string(),
            })?
            .join(path)
    };

    let mut resolved = PathBuf::new();
    let mut pending = absolute.components().peekable();

    while let Some(component) = pending.next() {
        match component {
            Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(segment) => {
                let candidate = resolved.join(segment);
                match std::fs::symlink_metadata(&candidate) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        let target = std::fs::read_link(&candidate).map_err(|error| {
                            PathSafetyError::CanonicalizeFailed {
                                path: candidate.clone(),
                                reason: error.to_string(),
                            }
                        })?;
                        let absolute_target = if target.is_absolute() {
                            target
                        } else {
                            resolved.join(target)
                        };

                        resolved = canonicalize_allow_missing(&absolute_target)?;
                    }
                    Ok(_) => resolved.push(segment),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        resolved.push(segment);
                        for remainder in pending {
                            match remainder {
                                Component::CurDir => {}
                                Component::ParentDir => {
                                    resolved.pop();
                                }
                                Component::Normal(next) => resolved.push(next),
                                Component::RootDir => {}
                                Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
                            }
                        }
                        break;
                    }
                    Err(error) => {
                        return Err(PathSafetyError::CanonicalizeFailed {
                            path: candidate,
                            reason: error.to_string(),
                        });
                    }
                }
            }
        }
    }

    Ok(normalize_path_buf(&resolved))
}

pub fn validate_workspace_path(
    workspace_root: &Path,
    workspace_path: &Path,
) -> Result<PathBuf, PathSafetyError> {
    let expanded_root = canonicalize_allow_missing(workspace_root)?;
    let expanded_workspace = canonicalize_allow_missing(workspace_path)?;

    if expanded_workspace == expanded_root {
        return Err(PathSafetyError::WorkspaceRoot);
    }

    if expanded_workspace.starts_with(&expanded_root) {
        return Ok(expanded_workspace);
    }

    let lexical_root = normalize_path_buf(&absolutize(workspace_root));
    let lexical_workspace = normalize_path_buf(&absolutize(workspace_path));

    if lexical_workspace.starts_with(&lexical_root) {
        return Err(PathSafetyError::SymlinkEscape {
            workspace: lexical_workspace.to_string_lossy().into_owned(),
            root: expanded_root.to_string_lossy().into_owned(),
        });
    }

    Err(PathSafetyError::OutsideWorkspaceRoot {
        workspace: expanded_workspace.to_string_lossy().into_owned(),
        root: expanded_root.to_string_lossy().into_owned(),
    })
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn normalize_path_buf(path: &Path) -> PathBuf {
    let mut components = Vec::<OsString>::new();
    let mut root = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => root.push(prefix.as_os_str()),
            Component::RootDir => root.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                components.pop();
            }
            Component::Normal(segment) => components.push(segment.to_os_string()),
        }
    }

    components.into_iter().fold(root, |mut acc, segment| {
        acc.push(segment);
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn canonicalize_allows_missing_suffix() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing").join("child");
        let canonical = canonicalize_allow_missing(&path).unwrap();
        assert!(canonical.ends_with(Path::new("missing").join("child")));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("link")).unwrap();

        let error = validate_workspace_path(&root, &root.join("link").join("child")).unwrap_err();
        assert!(matches!(error, PathSafetyError::SymlinkEscape { .. }));
    }
}
