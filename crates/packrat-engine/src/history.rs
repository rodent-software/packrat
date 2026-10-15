//! All-time backup history, persisted next to the preferences.
//!
//! Unlike the library scan, this is a running tally of what packrat itself has
//! written, so it survives deleting or re-ripping a file. The file mirrors
//! `config.toml`'s tiny `key = value` style and can be hand-edited; unknown
//! keys and malformed lines are ignored.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// Running totals of the rips packrat has completed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    /// Rip runs that wrote at least one file.
    pub discs: u64,
    /// Output files written.
    pub files: u64,
    /// Payload bytes written, excluding container overhead.
    pub bytes: u64,
    /// Unix time of the last completed rip, when there has been one.
    pub last_backup: Option<u64>,
}

impl History {
    /// Load the saved history, falling back to an empty tally when the file is
    /// missing or unreadable.
    pub fn load() -> History {
        let Some(path) = history_path() else {
            return History::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => History::parse(&text),
            Err(_) => History::default(),
        }
    }

    /// Parse the file body. Unrecognised keys and malformed lines are ignored.
    pub fn parse(text: &str) -> History {
        let mut history = History::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "discs" => history.discs = value.trim().parse().unwrap_or(0),
                "files" => history.files = value.trim().parse().unwrap_or(0),
                "bytes" => history.bytes = value.trim().parse().unwrap_or(0),
                "last_backup" => history.last_backup = value.trim().parse().ok(),
                _ => {}
            }
        }
        history
    }

    /// Serialize to the on-disk format.
    pub fn render(&self) -> String {
        let mut out = String::from("# packrat backup history\n");
        out.push_str(&format!("discs = {}\n", self.discs));
        out.push_str(&format!("files = {}\n", self.files));
        out.push_str(&format!("bytes = {}\n", self.bytes));
        if let Some(last_backup) = self.last_backup {
            out.push_str(&format!("last_backup = {last_backup}\n"));
        }
        out
    }

    /// Write the history, creating its directory.
    pub fn save(&self) -> Result<()> {
        let Some(path) = history_path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, self.render())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Fold one completed rip into the tally. A run that wrote nothing (every
    /// file failed or was cancelled) is not recorded.
    pub fn record(&mut self, files: u64, bytes: u64, now: u64) {
        if files == 0 {
            return;
        }
        self.discs = self.discs.saturating_add(1);
        self.files = self.files.saturating_add(files);
        self.bytes = self.bytes.saturating_add(bytes);
        self.last_backup = Some(now);
    }
}

/// Current Unix time in whole seconds.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Path of the history file, e.g. `~/.config/packrat/history.toml`.
pub fn history_path() -> Option<PathBuf> {
    Some(
        crate::config::config_dir()?
            .join("packrat")
            .join("history.toml"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_round_trips() {
        let text = "\
# a comment
discs = 3
files = 41
bytes = 18412345678
last_backup = 1790533088
unknown = nope
";
        let history = History::parse(text);
        assert_eq!(history.discs, 3);
        assert_eq!(history.files, 41);
        assert_eq!(history.bytes, 18_412_345_678);
        assert_eq!(history.last_backup, Some(1_790_533_088));
        assert_eq!(History::parse(&history.render()), history);
    }

    #[test]
    fn ignores_junk_and_missing_timestamps() {
        let history = History::parse("\n# hi\nnot a setting\nfiles = abc\n");
        assert_eq!(history, History::default());
        assert_eq!(history.last_backup, None);
    }

    #[test]
    fn record_ignores_an_empty_rip() {
        let mut history = History::default();
        history.record(0, 0, 100);
        assert_eq!(history, History::default());

        history.record(3, 1_000, 200);
        assert_eq!(history.discs, 1);
        assert_eq!(history.files, 3);
        assert_eq!(history.bytes, 1_000);
        assert_eq!(history.last_backup, Some(200));

        history.record(2, 500, 300);
        assert_eq!(history.discs, 2);
        assert_eq!(history.files, 5);
        assert_eq!(history.bytes, 1_500);
        assert_eq!(history.last_backup, Some(300));
    }
}
