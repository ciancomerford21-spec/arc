//! File-path guard for Arc's file tools.
//!
//! [`PathGuard::check`] turns a user/model-supplied path into a canonical
//! absolute path and refuses it when it:
//! * is empty or contains a NUL byte;
//! * resolves (after following symlinks) outside every `files.allowed_roots`;
//! * is inside a `files.sensitive_paths` entry (SSH keys, keyrings, browser
//!   profiles, Arc's own secrets…);
//! * is a write/delete aimed at an allowed root itself (e.g. "delete ~").
//!
//! Symlinks are resolved on the longest existing prefix, so a not-yet-created
//! destination is still checked against where it would really land. The guard
//! does no I/O besides `canonicalize` and never runs anything.

use arc_config::Files;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    Invalid(String),
    OutsideRoots { path: PathBuf },
    Sensitive { path: PathBuf },
    RootItself { path: PathBuf },
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use arc_config::paths::display;
        match self {
            PathError::Empty => f.write_str("no path given"),
            PathError::Invalid(why) => write!(f, "invalid path: {why}"),
            PathError::OutsideRoots { path } => {
                write!(f, "{} is outside the folders Arc may touch (files.allowed_roots)", display(path))
            }
            PathError::Sensitive { path } => {
                write!(f, "{} is a protected location (files.sensitive_paths)", display(path))
            }
            PathError::RootItself { path } => write!(f, "refusing to modify {} itself", display(path)),
        }
    }
}

impl std::error::Error for PathError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

#[derive(Debug, Clone)]
pub struct PathGuard {
    roots: Vec<PathBuf>,
    sensitive: Vec<PathBuf>,
    base: PathBuf,
}

impl PathGuard {
    pub fn from_config(files: &Files) -> Self {
        Self::new(
            files.allowed_roots.iter().map(|s| arc_config::paths::expand(s)),
            files.sensitive_paths.iter().map(|s| arc_config::paths::expand(s)),
            arc_config::paths::home_dir(),
        )
    }

    /// `base` is where relative paths are resolved from (the home directory
    /// in production).
    pub fn new(
        roots: impl IntoIterator<Item = PathBuf>,
        sensitive: impl IntoIterator<Item = PathBuf>,
        base: PathBuf,
    ) -> Self {
        Self {
            roots: roots.into_iter().map(|p| resolve(&p)).collect(),
            sensitive: sensitive.into_iter().map(|p| resolve(&p)).collect(),
            base,
        }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Resolve and validate `input` for the given access kind.
    pub fn check(&self, input: &str, access: Access) -> Result<PathBuf, PathError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(PathError::Empty);
        }
        if input.contains('\0') {
            return Err(PathError::Invalid("contains a NUL byte".into()));
        }
        let expanded = arc_config::paths::expand(input);
        let abs = if expanded.is_absolute() { expanded } else { self.base.join(expanded) };
        let path = resolve(&abs);

        if self.is_sensitive_resolved(&path) {
            return Err(PathError::Sensitive { path });
        }
        if !self.roots.iter().any(|r| path.starts_with(r)) {
            return Err(PathError::OutsideRoots { path });
        }
        if access == Access::Write && self.roots.iter().any(|r| &path == r) {
            return Err(PathError::RootItself { path });
        }
        Ok(path)
    }

    /// For filtering search results: true if `path` is (inside) a sensitive
    /// location. `path` should already be absolute.
    pub fn is_sensitive(&self, path: &Path) -> bool {
        self.is_sensitive_resolved(&resolve(path))
    }

    fn is_sensitive_resolved(&self, path: &Path) -> bool {
        self.sensitive.iter().any(|s| path.starts_with(s))
    }
}

/// Lexically normalise `p` (drop `.`, apply `..`), then canonicalise the
/// longest prefix that exists so symlinks are followed, re-appending the
/// non-existent remainder.
fn resolve(p: &Path) -> PathBuf {
    let norm = normalize(p);
    let mut existing = norm.clone();
    let mut rest: Vec<std::ffi::OsString> = vec![];
    loop {
        if let Ok(c) = existing.canonicalize() {
            let mut out = c;
            for part in rest.iter().rev() {
                out.push(part);
            }
            // The remainder may itself contain `..` after symlink resolution;
            // it can't here because `normalize` already removed them.
            return out;
        }
        match (existing.file_name().map(|f| f.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return norm,
        }
    }
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() { PathBuf::from("/") } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fx {
        _dir: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
        guard: PathGuard,
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("home");
        let outside = base.join("elsewhere");
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(root.join(".ssh")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join(".ssh/id_ed25519"), "k").unwrap();
        fs::write(outside.join("secret.txt"), "x").unwrap();
        let guard = PathGuard::new([root.clone()], [root.join(".ssh")], root.clone());
        Fx { _dir: dir, root, outside, guard }
    }

    #[test]
    fn inside_root_ok_and_relative_resolved_from_base() {
        let f = fx();
        assert_eq!(f.guard.check("docs", Access::Read).unwrap(), f.root.join("docs"));
        let abs = f.root.join("docs/new.txt");
        assert_eq!(f.guard.check(abs.to_str().unwrap(), Access::Write).unwrap(), abs);
    }

    #[test]
    fn outside_root_refused() {
        let f = fx();
        let p = f.outside.join("secret.txt");
        assert!(matches!(
            f.guard.check(p.to_str().unwrap(), Access::Read),
            Err(PathError::OutsideRoots { .. })
        ));
        assert!(matches!(
            f.guard.check("../elsewhere/secret.txt", Access::Read),
            Err(PathError::OutsideRoots { .. })
        ));
        assert!(matches!(f.guard.check("/etc/passwd", Access::Read), Err(PathError::OutsideRoots { .. })));
    }

    #[test]
    fn symlink_escape_refused() {
        let f = fx();
        std::os::unix::fs::symlink(&f.outside, f.root.join("link")).unwrap();
        assert!(matches!(
            f.guard.check("link/secret.txt", Access::Read),
            Err(PathError::OutsideRoots { .. })
        ));
        // Non-existent file under the escaping link is also caught.
        assert!(matches!(f.guard.check("link/new.txt", Access::Write), Err(PathError::OutsideRoots { .. })));
    }

    #[test]
    fn sensitive_refused_including_via_symlink() {
        let f = fx();
        assert!(matches!(f.guard.check(".ssh/id_ed25519", Access::Read), Err(PathError::Sensitive { .. })));
        assert!(matches!(f.guard.check("docs/../.ssh", Access::Read), Err(PathError::Sensitive { .. })));
        std::os::unix::fs::symlink(f.root.join(".ssh"), f.root.join("docs/keys")).unwrap();
        assert!(matches!(
            f.guard.check("docs/keys/id_ed25519", Access::Read),
            Err(PathError::Sensitive { .. })
        ));
        assert!(f.guard.is_sensitive(&f.root.join(".ssh/config")));
        assert!(!f.guard.is_sensitive(&f.root.join("docs")));
    }

    #[test]
    fn writing_a_root_itself_refused() {
        let f = fx();
        let r = f.root.to_str().unwrap();
        assert!(f.guard.check(r, Access::Read).is_ok());
        assert!(matches!(f.guard.check(r, Access::Write), Err(PathError::RootItself { .. })));
    }

    #[test]
    fn empty_and_nul() {
        let f = fx();
        assert_eq!(f.guard.check("  ", Access::Read), Err(PathError::Empty));
        assert!(matches!(f.guard.check("a\0b", Access::Read), Err(PathError::Invalid(_))));
    }

    #[test]
    fn from_default_config() {
        let g = PathGuard::from_config(&Files::default());
        let home = arc_config::paths::home_dir();
        assert!(g.check("~/Downloads", Access::Read).is_ok() || !home.exists());
        assert!(matches!(g.check("~/.ssh/id_rsa", Access::Read), Err(PathError::Sensitive { .. })));
        assert!(matches!(g.check("/etc/shadow", Access::Read), Err(PathError::OutsideRoots { .. })));
    }
}
