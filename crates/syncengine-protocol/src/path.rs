//! Relative-path safety for shared folders.
//!
//! Every path exchanged over the protocol or written to disk is a
//! *relative* path inside a shared folder root. This module is the single
//! chokepoint that rejects traversal, absolute paths, Windows prefixes, and
//! other escapes before any filesystem call (SRD §13.2, FR-04).

use serde::{Deserialize, Serialize};

/// Errors produced by [`validate_relative_path`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("path is empty")]
    Empty,
    #[error("path is absolute: {0}")]
    Absolute(String),
    #[error("path contains a parent-directory component: {0}")]
    ParentTraversal(String),
    #[error("path contains a Windows prefix or root: {0}")]
    WindowsPrefix(String),
    #[error("path contains a NUL byte")]
    NulByte,
    #[error("path contains backslashes; use forward slashes: {0}")]
    Backslash(String),
}

/// A validated, forward-slash relative path inside a shared folder.
///
/// Guaranteed invariants after validation:
/// - non-empty
/// - forward slashes only
/// - no absolute prefix (Unix `/`, Windows `C:\`, UNC `\\`)
/// - no `.` or `..` components
/// - no NUL bytes
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RelativePath(String);

impl RelativePath {
    /// Validates `raw` and returns a `RelativePath`.
    pub fn validate(raw: &str) -> Result<Self, PathError> {
        if raw.is_empty() {
            return Err(PathError::Empty);
        }
        if raw.contains('\0') {
            return Err(PathError::NulByte);
        }
        if raw.contains('\\') {
            return Err(PathError::Backslash(raw.to_string()));
        }
        if raw.starts_with('/') {
            return Err(PathError::Absolute(raw.to_string()));
        }
        // Windows drive letters ("C:") and UNC-style prefixes.
        let bytes = raw.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' {
            return Err(PathError::WindowsPrefix(raw.to_string()));
        }

        for component in raw.split('/') {
            if component.is_empty() {
                // "a//b" or a trailing slash — reject rather than silently
                // normalizing, so peers cannot smuggle alternate spellings
                // of the same path past uniqueness checks.
                return Err(PathError::ParentTraversal(raw.to_string()));
            }
            if component == "." || component == ".." {
                return Err(PathError::ParentTraversal(raw.to_string()));
            }
        }

        Ok(Self(raw.to_string()))
    }

    /// The validated path as a `&str` with forward slashes.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Splits into parent directory (None for top-level) and file name.
    pub fn parent_and_file(&self) -> (Option<&str>, &str) {
        match self.0.rsplit_once('/') {
            Some((parent, file)) => (Some(parent), file),
            None => (None, &self.0),
        }
    }

    /// All ancestor directories, outermost first (empty for top-level files).
    pub fn ancestors(&self) -> Vec<&str> {
        let (parent, _) = self.parent_and_file();
        match parent {
            Some(parent) => parent.split('/').collect(),
            None => Vec::new(),
        }
    }
}

impl std::fmt::Display for RelativePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RelativePath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) -> RelativePath {
        RelativePath::validate(raw).expect("valid")
    }

    fn err(raw: &str) -> PathError {
        RelativePath::validate(raw).unwrap_err()
    }

    #[test]
    fn accepts_ordinary_relative_paths() {
        assert_eq!(ok("a.txt").as_str(), "a.txt");
        assert_eq!(ok("docs/readme.md").as_str(), "docs/readme.md");
        assert_eq!(ok("a/b/c/d.bin").as_str(), "a/b/c/d.bin");
    }

    #[test]
    fn accepts_unicode_names() {
        assert_eq!(ok("фото/лето.jpg").as_str(), "фото/лето.jpg");
    }

    #[test]
    fn rejects_empty_and_nul() {
        assert_eq!(err(""), PathError::Empty);
        assert_eq!(err("a\0b"), PathError::NulByte);
    }

    #[test]
    fn rejects_absolute_unix_paths() {
        assert!(matches!(err("/etc/passwd"), PathError::Absolute(_)));
    }

    #[test]
    fn rejects_windows_prefixes() {
        assert!(matches!(
            err("C:/Windows/system32"),
            PathError::WindowsPrefix(_)
        ));
        assert!(matches!(err("c:x"), PathError::WindowsPrefix(_)));
    }

    #[test]
    fn rejects_backslashes() {
        assert!(matches!(err("a\\b"), PathError::Backslash(_)));
    }

    #[test]
    fn rejects_parent_traversal() {
        assert!(matches!(err("../secret"), PathError::ParentTraversal(_)));
        assert!(matches!(err("a/../b"), PathError::ParentTraversal(_)));
        assert!(matches!(err("a/.."), PathError::ParentTraversal(_)));
        assert!(matches!(err(".."), PathError::ParentTraversal(_)));
    }

    #[test]
    fn rejects_dot_and_double_slash() {
        assert!(matches!(err("./a"), PathError::ParentTraversal(_)));
        assert!(matches!(err("a//b"), PathError::ParentTraversal(_)));
        assert!(matches!(err("a/"), PathError::ParentTraversal(_)));
    }

    #[test]
    fn rejects_parent_component_even_next_to_unicode() {
        // Only ASCII '/' separates components; an ASCII ".." component is
        // rejected regardless of surrounding unicode characters.
        assert!(matches!(err("a/..∕b/../c"), PathError::ParentTraversal(_)));
        assert!(
            RelativePath::validate("a∕b").is_ok(),
            "unicode slash is literal"
        );
    }

    #[test]
    fn parent_and_file_split() {
        assert_eq!(ok("a.txt").parent_and_file(), (None, "a.txt"));
        assert_eq!(ok("d/a.txt").parent_and_file(), (Some("d"), "a.txt"));
        assert_eq!(ok("a/b/c.txt").parent_and_file(), (Some("a/b"), "c.txt"));
    }

    #[test]
    fn ancestors_listed_outermost_first() {
        assert!(ok("a.txt").ancestors().is_empty());
        assert_eq!(ok("d/a.txt").ancestors(), vec!["d"]);
        assert_eq!(ok("a/b/c.txt").ancestors(), vec!["a", "b"]);
    }
}
