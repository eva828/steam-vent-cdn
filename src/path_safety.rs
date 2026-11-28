//! Validation for names that come from Steam metadata and are joined onto an
//! output directory.
//!
//! Manifest entries, the PICS install directory and workshop file names are all
//! written below `--output`, so absolute paths and `..` components must never
//! reach `Path::join`.

use std::path::Component;

/// Whether `name` is a plain relative path.
///
/// Empty names, NUL characters, absolute paths (including Windows drive
/// letters) and any `.`/`..` component are rejected.
pub fn is_relative(name: &str) -> bool {
    if name.is_empty() || name.contains('\0') {
        return false;
    }

    // Windows drive letters are not a `Component::Prefix` on other platforms, so
    // reject them explicitly to keep behaviour identical everywhere.
    if name.as_bytes().get(1) == Some(&b':') {
        return false;
    }

    std::path::Path::new(name)
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::is_relative;

    #[test]
    fn accepts_nested_relative_paths() {
        assert!(is_relative("bin/game.exe"));
        assert!(is_relative("a"));
        assert!(is_relative("a//b"));
    }

    #[test]
    fn rejects_empty_and_nul() {
        assert!(!is_relative(""));
        assert!(!is_relative("a\0b"));
    }

    #[test]
    fn rejects_absolute_paths() {
        assert!(!is_relative("/etc/passwd"));
        assert!(!is_relative("C:/Windows/system32"));
        assert!(!is_relative("c:Windows"));
    }

    #[test]
    fn rejects_parent_and_current_components() {
        assert!(!is_relative(".."));
        assert!(!is_relative("../.bashrc"));
        assert!(!is_relative("a/../../b"));
        assert!(!is_relative("./a"));
    }
}
