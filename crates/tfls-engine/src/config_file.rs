//! Shared project config file (`.tfls.json`).
//!
//! Lets a repo check in the same `rules` / `styleRules` / `formatStyle`
//! policy the LSP `initializationOptions` / `workspace/didChangeConfiguration`
//! accept, so the editor and CI (`tfls-lint`) agree without configuring each
//! side separately.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Filename `find_config_file` looks for. The file's content is exactly the
/// `terraform-ls-rs` settings object (`{"rules": {...}, "styleRules": true,
/// "formatStyle": "minimal"}`) — no wrapper key.
pub const CONFIG_FILE_NAME: &str = ".tfls.json";

/// Look for [`CONFIG_FILE_NAME`] in `start` and each ancestor directory,
/// stopping at the filesystem root. Returns the first hit — the nearest
/// ancestor wins, matching how `.gitignore`-style project files resolve.
/// No git-root heuristic: a config file above the repo root (if any) is
/// found just like one inside it.
pub fn find_config_file(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// Failure loading a discovered config file. One variant per failure site so
/// a caller can tell which step failed from the variant alone.
#[derive(Debug, thiserror::Error)]
pub enum ConfigFileError {
    #[error("failed to read config file '{path}'")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse config file '{path}' as JSON")]
    Parse {
        path: PathBuf,
        #[source]
        source: sonic_rs::Error,
    },
    #[error("config file '{path}' must contain a JSON object")]
    NotAnObject { path: PathBuf },
}

/// Load and parse `path` as the `terraform-ls-rs` settings object.
pub fn load_config_file(path: &Path) -> Result<sonic_rs::Value, ConfigFileError> {
    use sonic_rs::JsonValueTrait;

    let content = fs::read_to_string(path).map_err(|source| ConfigFileError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let value: sonic_rs::Value =
        sonic_rs::from_str(&content).map_err(|source| ConfigFileError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    if !value.is_object() {
        return Err(ConfigFileError::NotAnObject {
            path: path.to_path_buf(),
        });
    }
    Ok(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use sonic_rs::JsonValueTrait;
    use tempfile::tempdir;

    #[test]
    fn finds_config_file_in_start_dir() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join(CONFIG_FILE_NAME), "{}").expect("write");

        let found = find_config_file(dir.path());
        assert_eq!(found, Some(dir.path().join(CONFIG_FILE_NAME)));
    }

    #[test]
    fn finds_config_file_in_grandparent_dir() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        let leaf = root.join("a").join("b");
        fs::create_dir_all(&leaf).expect("mkdir");
        fs::write(root.join(CONFIG_FILE_NAME), "{}").expect("write");

        let found = find_config_file(&leaf);
        assert_eq!(found, Some(root.join(CONFIG_FILE_NAME)));
    }

    #[test]
    fn returns_none_when_no_config_file_anywhere() {
        let dir = tempdir().expect("tempdir");
        let leaf = dir.path().join("a").join("b");
        fs::create_dir_all(&leaf).expect("mkdir");

        assert_eq!(find_config_file(&leaf), None);
    }

    #[test]
    fn load_reports_parse_error() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join(CONFIG_FILE_NAME);
        fs::write(&path, "{ not json").expect("write");

        let err = load_config_file(&path).expect_err("should fail to parse");
        assert!(matches!(err, ConfigFileError::Parse { .. }));
    }

    #[test]
    fn load_rejects_non_object_array() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join(CONFIG_FILE_NAME);
        fs::write(&path, "[]").expect("write");

        let err = load_config_file(&path).expect_err("array is not an object");
        assert!(matches!(err, ConfigFileError::NotAnObject { .. }));
    }

    #[test]
    fn load_reports_read_error_for_missing_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join(CONFIG_FILE_NAME);

        let err = load_config_file(&path).expect_err("file does not exist");
        assert!(matches!(err, ConfigFileError::Read { .. }));
    }

    #[test]
    fn load_parses_valid_object() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join(CONFIG_FILE_NAME);
        fs::write(&path, r#"{"styleRules": true}"#).expect("write");

        let value = load_config_file(&path).expect("should parse");
        assert!(value.is_object());
    }
}
