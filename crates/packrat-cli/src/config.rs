//! User preferences, persisted between runs.
//!
//! This is deliberately a tiny `key = "value"` file rather than a general
//! parser: packrat only remembers two directory preferences. Unknown keys and
//! malformed lines are ignored so a hand-edited file never breaks a run.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Where packrat writes files, by media type.
///
/// Each directory is the folder that *holds* show or movie folders, so an
/// existing library (`/mnt/media/tv`, `/mnt/media/movies`) can be used as-is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// Directory holding show folders, e.g. `/mnt/dvd/media/tv`.
    pub tv_dir: Option<PathBuf>,
    /// Directory holding movie folders, e.g. `/mnt/dvd/media/movies`.
    pub movie_dir: Option<PathBuf>,
}

impl Config {
    /// Load saved preferences, falling back to defaults when the file is
    /// missing or unreadable.
    pub fn load() -> Config {
        let Some(path) = config_path() else {
            return Config::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text),
            Err(_) => Config::default(),
        }
    }

    /// Parse the file body. Unrecognised keys and malformed lines are ignored.
    pub fn parse(text: &str) -> Config {
        let mut config = Config::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = unquote(value.trim());
            if value.is_empty() {
                continue;
            }
            match key.trim() {
                "tv_dir" => config.tv_dir = Some(expand_tilde(&value)),
                "movie_dir" => config.movie_dir = Some(expand_tilde(&value)),
                _ => {}
            }
        }
        config
    }

    /// Serialize to the on-disk format.
    pub fn render(&self) -> String {
        let mut out = String::from("# packrat preferences\n");
        if let Some(dir) = &self.tv_dir {
            out.push_str(&format!(
                "tv_dir = \"{}\"\n",
                escape(&dir.to_string_lossy())
            ));
        }
        if let Some(dir) = &self.movie_dir {
            out.push_str(&format!(
                "movie_dir = \"{}\"\n",
                escape(&dir.to_string_lossy())
            ));
        }
        out
    }

    /// Write the config, creating its directory. Returns whether the file
    /// changed (so the UI can confirm a save).
    pub fn save(&self) -> Result<bool> {
        let Some(path) = config_path() else {
            return Ok(false);
        };
        let text = self.render();
        if std::fs::read_to_string(&path).is_ok_and(|old| old == text) {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(true)
    }
}

/// Path of the preferences file, e.g. `~/.config/packrat/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("packrat").join("config.toml"))
}

/// Platform configuration directory.
fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }
}

/// Expand a leading `~/` using `$HOME`.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    let inner = if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    unescape(inner)
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parses_and_round_trips() {
        let text = "\
# a comment
tv_dir = \"/mnt/dvd/media/tv\"
movie_dir = '/mnt/dvd/media/movies'
unknown = 3
";
        let config = Config::parse(text);
        assert_eq!(
            config.tv_dir.as_deref(),
            Some(Path::new("/mnt/dvd/media/tv"))
        );
        assert_eq!(
            config.movie_dir.as_deref(),
            Some(Path::new("/mnt/dvd/media/movies"))
        );
        assert_eq!(Config::parse(&config.render()), config);
    }

    #[test]
    fn escapes_quotes_and_backslashes() {
        let config = Config {
            tv_dir: Some(PathBuf::from("C:\\Media\\TV \"x\"")),
            movie_dir: None,
        };
        assert_eq!(Config::parse(&config.render()), config);
    }

    #[test]
    fn ignores_junk_and_empty_values() {
        let config = Config::parse("\n# hi\nnot a setting\ntv_dir =\nmovie_dir = /abs/path\n");
        assert_eq!(config.tv_dir, None);
        assert_eq!(config.movie_dir.as_deref(), Some(Path::new("/abs/path")));
    }

    #[test]
    fn expands_tilde_and_passes_absolute_paths_through() {
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(expand_tilde("~/media"), PathBuf::from(home).join("media"));
        }
        assert_eq!(expand_tilde("/mnt/media"), PathBuf::from("/mnt/media"));
    }
}
