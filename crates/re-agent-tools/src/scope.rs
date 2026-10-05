//! Which files an agent may touch without asking.
//!
//! The scope is a folder (the file's repository root, or its folder).
//! Paths are resolved lexically for display and canonicalized (following
//! symlinks) to decide whether they are inside the scope, so a link that
//! points outside counts as outside.

use std::path::{Component, Path, PathBuf};

/// The default scope for a file: the enclosing repository (a folder with
/// `.git`), else the file's folder.
pub fn default_scope(file: &Path) -> Option<PathBuf> {
    let folder = file.parent()?;
    let mut dir = Some(folder);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    Some(folder.to_path_buf())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// As written, made absolute (for messages).
    pub display: PathBuf,
    /// Canonical form (or the closest one possible for new files): use this
    /// for I/O.
    pub path: PathBuf,
    pub inside: bool,
}

fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalizes the deepest existing ancestor and appends the rest.
fn canonical(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            let mut out = canonical;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (
            existing.file_name().map(|n| n.to_os_string()),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Resolves `raw` (absolute, or relative to the scope).
pub fn resolve(scope: Option<&Path>, raw: &str) -> Result<Resolved, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("the path is empty".into());
    }
    let given = Path::new(raw);
    let absolute = if given.is_absolute() {
        given.to_path_buf()
    } else {
        scope
            .ok_or("this conversation has no folder; use an absolute path")?
            .join(given)
    };
    let display = lexical(&absolute);
    let path = canonical(&display);
    let inside = scope.is_some_and(|s| path.starts_with(canonical(s)));
    Ok(Resolved {
        display,
        path,
        inside,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    #[test]
    fn relative_paths_resolve_inside_and_escapes_are_outside() {
        let root = temp_dir("scope");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        let inside = resolve(Some(&root), "src/a.rs").unwrap();
        assert!(inside.inside);
        assert!(resolve(Some(&root), "src/new/file.txt").unwrap().inside);
        assert!(!resolve(Some(&root), "../elsewhere.txt").unwrap().inside);
        assert!(!resolve(Some(&root), "src/../../x").unwrap().inside);
        let absolute = std::env::temp_dir().join("re-agent-other.txt");
        assert!(
            !resolve(Some(&root), &absolute.to_string_lossy())
                .unwrap()
                .inside
        );
        assert!(resolve(None, "relative.txt").is_err());
        assert!(resolve(Some(&root), "  ").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_pointing_outside_are_outside() {
        let root = temp_dir("link");
        let outside = temp_dir("target");
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert!(!resolve(Some(&root), "link/secret.txt").unwrap().inside);
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn default_scope_is_the_repository_root() {
        let root = temp_dir("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        assert_eq!(default_scope(&root.join("a/b/c.txt")), Some(root.clone()));
        std::fs::remove_dir_all(root.join(".git")).unwrap();
        assert_eq!(
            default_scope(&root.join("a/b/c.txt")),
            Some(root.join("a/b"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
