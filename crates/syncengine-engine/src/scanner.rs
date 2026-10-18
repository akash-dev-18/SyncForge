//! Filesystem scanning and reconciliation input (LLD `Reconciler`).
//!
//! A scan walks a shared-folder root without following symlinks and produces
//! the current set of present files and directories, each identified by a
//! validated forward-slash relative path. Scans are *conservative*: any
//! error encountered while walking marks the scan incomplete so the caller
//! must not interpret missing entries as deletions (SRD FR-05).

use std::path::{Path, PathBuf};

use syncengine_protocol::path::RelativePath;

use crate::error::EngineError;
use crate::integrity::hash_file;

/// One present file discovered during a scan.
#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub relative_path: RelativePath,
    pub absolute_path: PathBuf,
    pub size: u64,
    pub modified_at: Option<String>,
    pub content_hash: syncengine_protocol::ContentHash,
}

/// A path that was skipped, with the reason.
#[derive(Debug, Clone)]
pub struct SkippedEntry {
    pub path: String,
    pub reason: String,
}

/// The outcome of scanning one root.
#[derive(Debug, Default)]
pub struct ScanResult {
    pub files: Vec<ScannedFile>,
    pub directories: Vec<RelativePath>,
    pub skipped: Vec<SkippedEntry>,
    /// `false` when some part of the tree could not be read reliably.
    pub complete: bool,
}

impl ScanResult {
    /// Present file paths, sorted for deterministic comparison.
    pub fn sorted_files(&self) -> Vec<&ScannedFile> {
        let mut files: Vec<&ScannedFile> = self.files.iter().collect();
        files.sort_by(|a, b| a.relative_path.as_str().cmp(b.relative_path.as_str()));
        files
    }
}

/// Scans `root`, returning present files and directories.
pub fn scan_root(root: &Path) -> Result<ScanResult, EngineError> {
    if !root.is_dir() {
        return Err(EngineError::Path(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    let mut result = ScanResult {
        complete: true,
        ..ScanResult::default()
    };
    walk(root, root, &mut result);
    Ok(result)
}

fn walk(root: &Path, dir: &Path, result: &mut ScanResult) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            result.complete = false;
            result.skipped.push(SkippedEntry {
                path: display_relative(root, dir),
                reason: format!("cannot read directory: {e}"),
            });
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                result.complete = false;
                result.skipped.push(SkippedEntry {
                    path: display_relative(root, dir),
                    reason: format!("cannot read entry: {e}"),
                });
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                result.complete = false;
                result.skipped.push(SkippedEntry {
                    path: display_relative(root, &path),
                    reason: format!("cannot stat: {e}"),
                });
                continue;
            }
        };

        let relative = match relative_string(root, &path) {
            Some(rel) => rel,
            None => continue,
        };

        if file_type.is_symlink() {
            result.skipped.push(SkippedEntry {
                path: relative,
                reason: "symlinks are not followed".to_string(),
            });
            continue;
        }

        if file_type.is_dir() {
            match RelativePath::validate(&relative) {
                Ok(dir_path) => result.directories.push(dir_path),
                Err(e) => {
                    result.skipped.push(SkippedEntry {
                        path: relative,
                        reason: format!("invalid directory path: {e}"),
                    });
                    continue;
                }
            }
            walk(root, &path, result);
            continue;
        }

        if file_type.is_file() {
            add_file(&relative, &path, result);
            continue;
        }

        result.skipped.push(SkippedEntry {
            path: relative,
            reason: "not a regular file or directory".to_string(),
        });
    }
}

fn add_file(relative: &str, path: &Path, result: &mut ScanResult) {
    let relative_path = match RelativePath::validate(relative) {
        Ok(p) => p,
        Err(e) => {
            result.skipped.push(SkippedEntry {
                path: relative.to_string(),
                reason: format!("invalid file path: {e}"),
            });
            return;
        }
    };

    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            result.complete = false;
            result.skipped.push(SkippedEntry {
                path: relative.to_string(),
                reason: format!("cannot stat file: {e}"),
            });
            return;
        }
    };

    let content_hash = match hash_file(path) {
        Ok(hash) => hash,
        Err(e) => {
            result.complete = false;
            result.skipped.push(SkippedEntry {
                path: relative.to_string(),
                reason: format!("cannot hash file: {e}"),
            });
            return;
        }
    };

    result.files.push(ScannedFile {
        relative_path,
        absolute_path: path.to_path_buf(),
        size: metadata.len(),
        modified_at: metadata.modified().ok().map(format_system_time),
        content_hash,
    });
}

/// Computes the forward-slash relative path of `path` under `root`, or `None`
/// if `path` is not actually inside `root`.
fn relative_string(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        let s = component.as_os_str().to_str()?;
        parts.push(s);
    }
    Some(parts.join("/"))
}

fn display_relative(root: &Path, path: &Path) -> String {
    relative_string(root, path).unwrap_or_else(|| path.display().to_string())
}

fn format_system_time(time: std::time::SystemTime) -> String {
    let odt = time::OffsetDateTime::from(time);
    odt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unix-ms:0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, contents: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn scans_files_and_directories_recursively() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", b"a");
        write(dir.path(), "docs/b.md", b"bb");
        write(dir.path(), "docs/deep/c.bin", b"ccc");

        let scan = scan_root(dir.path()).unwrap();
        assert!(scan.complete);
        assert_eq!(scan.files.len(), 3);
        let mut paths: Vec<&str> = scan
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["a.txt", "docs/b.md", "docs/deep/c.bin"]);

        let mut dirs: Vec<&str> = scan.directories.iter().map(|d| d.as_str()).collect();
        dirs.sort();
        assert_eq!(dirs, vec!["docs", "docs/deep"]);

        let deep = scan
            .files
            .iter()
            .find(|f| f.relative_path.as_str() == "docs/deep/c.bin")
            .unwrap();
        assert_eq!(deep.size, 3);
        assert_eq!(deep.content_hash, hash_file(&deep.absolute_path).unwrap());
    }

    #[test]
    fn rejects_non_directory_root() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "file.txt", b"x");
        assert!(scan_root(&dir.path().join("file.txt")).is_err());
    }

    #[test]
    fn empty_root_is_a_complete_empty_scan() {
        let dir = tempfile::tempdir().unwrap();
        let scan = scan_root(dir.path()).unwrap();
        assert!(scan.complete);
        assert!(scan.files.is_empty());
        assert!(scan.directories.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "real.txt", b"data");
        std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt"))
            .unwrap();
        let scan = scan_root(dir.path()).unwrap();
        let names: Vec<&str> = scan
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        assert_eq!(names, vec!["real.txt"]);
        assert!(
            scan.skipped
                .iter()
                .any(|s| s.path == "link.txt" && s.reason.contains("symlink"))
        );
    }
}
