//! User preferences, persisted between runs.
//!
//! This is deliberately a tiny `key = "value"` file rather than a general
//! parser: packrat only remembers one destination directory per media type.
//! Unknown keys and malformed lines are ignored so a hand-edited file never
//! breaks a run.

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
    /// TMDb API key or v4 read access token. Optional: without it, movies are
    /// named from the disc label. Stored in plaintext.
    pub tmdb_api_key: Option<String>,
    /// Optical drive last used in the interactive guide, e.g. `/dev/sr1`.
    pub last_device: Option<PathBuf>,
    /// Open the tray once a rip finishes without failures.
    pub auto_eject: bool,
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
                "tmdb_api_key" => config.tmdb_api_key = Some(value),
                "last_device" => config.last_device = Some(expand_tilde(&value)),
                "auto_eject" => config.auto_eject = parse_bool(&value),
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
        if let Some(key) = &self.tmdb_api_key {
            out.push_str(&format!("tmdb_api_key = \"{}\"\n", escape(key)));
        }
        if let Some(device) = &self.last_device {
            out.push_str(&format!(
                "last_device = \"{}\"\n",
                escape(&device.to_string_lossy())
            ));
        }
        if self.auto_eject {
            out.push_str("auto_eject = true\n");
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

    /// The TMDb key to use, if any.
    ///
    /// `TMDB_API_KEY` (or `PACKRAT_TMDB_KEY`) overrides the saved value, so a
    /// headless run can supply the key without touching the config file.
    pub fn tmdb_key(&self) -> Option<String> {
        let env = std::env::var("TMDB_API_KEY")
            .ok()
            .or_else(|| std::env::var("PACKRAT_TMDB_KEY").ok());
        key_precedence(self.tmdb_api_key.clone(), env)
    }
}

/// Prefer the environment key, then the saved one; blank values are ignored.
fn key_precedence(saved: Option<String>, env: Option<String>) -> Option<String> {
    let clean = |value: String| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    env.and_then(clean).or_else(|| saved.and_then(clean))
}

/// Path of the preferences file, e.g. `~/.config/packrat/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("packrat").join("config.toml"))
}

/// Whether preferences have been saved yet. A first-time user has no file, so
/// the guide can walk them through the settings screen before scanning.
pub fn exists() -> bool {
    config_path().is_some_and(|path| path.exists())
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

/// Read a generous set of truthy spellings; anything else is `false`.
fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
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
tmdb_api_key = \"abc123\"
last_device = \"/dev/sr1\"
auto_eject = true
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
        assert_eq!(config.tmdb_api_key.as_deref(), Some("abc123"));
        assert_eq!(config.last_device.as_deref(), Some(Path::new("/dev/sr1")));
        assert!(config.auto_eject);
        assert_eq!(Config::parse(&config.render()), config);
    }

    #[test]
    fn environment_key_overrides_the_saved_one() {
        assert_eq!(
            key_precedence(Some("saved".into()), Some("env".into())).as_deref(),
            Some("env")
        );
        assert_eq!(
            key_precedence(Some("saved".into()), None).as_deref(),
            Some("saved")
        );
        // A blank environment value falls through to the saved key.
        assert_eq!(
            key_precedence(Some("saved".into()), Some("   ".into())).as_deref(),
            Some("saved")
        );
        assert_eq!(key_precedence(None, Some("  ".into())), None);
    }

    #[test]
    fn parses_auto_eject_spellings() {
        for text in [
            "auto_eject = true",
            "auto_eject = 1",
            "auto_eject = yes",
            "auto_eject = on",
        ] {
            assert!(Config::parse(text).auto_eject, "{text:?}");
        }
        assert!(!Config::parse("auto_eject = false").auto_eject);
        assert!(!Config::parse("auto_eject =").auto_eject);
        assert!(!Config::default().auto_eject);
    }

    #[test]
    fn escapes_quotes_and_backslashes() {
        let config = Config {
            tv_dir: Some(PathBuf::from("C:\\Media\\TV \"x\"")),
            movie_dir: None,
            tmdb_api_key: Some("key.with.dots".into()),
            last_device: Some(PathBuf::from("\\\\.\\D:")),
            auto_eject: true,
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
