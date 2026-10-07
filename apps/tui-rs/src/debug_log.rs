//! Opt-in, size-bounded sidebar debug log.
//!
//! Every tmux window runs its own sidebar process and each one logs every
//! state update, so an always-on log grows without bound. Like the server,
//! the sidebar only logs when `OPENSESSIONS_DEBUG_LOG` names a path, and it
//! rotates the file to `<path>.1` once it reaches a size cap.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Rotate the log once it would grow past this many bytes. At most two files
/// (`<path>` and `<path>.1`) are kept, so disk use stays near twice this.
pub const DEBUG_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Resolve the debug log path. Logging is disabled unless
/// `OPENSESSIONS_DEBUG_LOG` is set to a non-blank path.
pub fn debug_log_path_from_env<F>(env: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    env("OPENSESSIONS_DEBUG_LOG")
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
}

/// Append `line` (plus a newline) to `path`, first rotating the current file
/// to `<path>.1` when the append would exceed `max_bytes`. Errors are
/// swallowed: debug logging must never disturb the sidebar.
pub fn append_bounded(path: &Path, line: &str, max_bytes: u64) {
    let incoming = line.len() as u64 + 1;
    if let Ok(metadata) = std::fs::metadata(path)
        && metadata.len() > 0
        && metadata.len() + incoming > max_bytes
    {
        let _ = std::fs::rename(path, rotated_path(path));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

pub fn rotated_path(path: &Path) -> PathBuf {
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    PathBuf::from(rotated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "opensessions-sidebar-debug-log-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn debug_log_is_disabled_unless_a_path_is_configured() {
        assert_eq!(debug_log_path_from_env(|_| None), None);
        assert_eq!(
            debug_log_path_from_env(|key| (key == "OPENSESSIONS_DEBUG_LOG").then(String::new)),
            None
        );
        assert_eq!(
            debug_log_path_from_env(|key| (key == "OPENSESSIONS_DEBUG_LOG").then(|| "  ".into())),
            None
        );
        assert_eq!(
            debug_log_path_from_env(
                |key| (key == "OPENSESSIONS_DEBUG_LOG").then(|| "/tmp/os.log".into())
            ),
            Some(PathBuf::from("/tmp/os.log"))
        );
    }

    #[test]
    fn debug_log_rotates_instead_of_growing_past_the_cap() {
        let dir = temp_dir("rotate");
        let path = dir.join("debug.log");
        let line = "x".repeat(99);

        for _ in 0..25 {
            append_bounded(&path, &line, 1_000);
        }

        let current = std::fs::metadata(&path).unwrap().len();
        let rotated = std::fs::metadata(rotated_path(&path)).unwrap().len();
        assert!(current <= 1_000, "current log grew to {current} bytes");
        assert!(rotated <= 1_000, "rotated log grew to {rotated} bytes");
        assert!(current > 0, "newest lines must stay in the current log");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
