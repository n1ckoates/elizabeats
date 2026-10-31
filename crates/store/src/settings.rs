//! `settings.json`: user Settings, read leniently.

use std::io;
use std::path::Path;

use elizabeats_core::{AppSettings, Theme};
use serde_json::Value;

use crate::{StoreError, write_atomic};

pub const SETTINGS_FILE: &str = "settings.json";

/// Reads Settings from `dir`. Never fails: anything unreadable falls back to
/// defaults.
pub fn load_settings(dir: &Path) -> AppSettings {
    match read_json(&dir.join(SETTINGS_FILE)) {
        Ok(Some(value)) => normalize_settings(&value),
        Ok(None) => AppSettings::default(),
        Err(error) => {
            tracing::warn!("Using default settings: {error}");
            AppSettings::default()
        }
    }
}

pub fn save_settings(dir: &Path, settings: &AppSettings) -> Result<(), StoreError> {
    let mut json = serde_json::to_string_pretty(settings).map_err(|source| StoreError::Encode {
        what: "Settings",
        source,
    })?;
    json.push('\n');
    write_atomic(&dir.join(SETTINGS_FILE), json.as_bytes())
}

/// `Ok(None)` when the file does not exist.
fn read_json(path: &Path) -> Result<Option<Value>, StoreError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(StoreError::io("read", path, source)),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| StoreError::Invalid {
            what: "Settings",
            detail: error.to_string(),
        })
}

/// Keeps each recognised, valid field and uses the default for the rest,
/// exactly like the Electron app's `normalizeSettings`.
pub fn normalize_settings(value: &Value) -> AppSettings {
    let mut settings = AppSettings::default();
    let Some(object) = value.as_object() else {
        return settings;
    };
    for (key, value) in object {
        match key.as_str() {
            "libraryFolder" => match value {
                Value::Null => settings.library_folder = None,
                Value::String(folder) => settings.library_folder = Some(folder.clone()),
                _ => {}
            },
            "theme" => match value.as_str() {
                Some("light") => settings.theme = Theme::Light,
                Some("dark") => settings.theme = Theme::Dark,
                Some("system") => settings.theme = Theme::System,
                _ => {}
            },
            "crossfadeDurationSeconds" => {
                if let Some(seconds) = integer_in(value, 0, AppSettings::MAX_CROSSFADE_SECONDS) {
                    settings.crossfade_duration_seconds = seconds;
                }
            }
            "downloadCacheSizeGiB" => {
                if let Some(gib) = integer_in(
                    value,
                    AppSettings::MIN_CACHE_GIB,
                    AppSettings::MAX_CACHE_GIB,
                ) {
                    settings.download_cache_size_gib = gib;
                }
            }
            _ => {}
        }
    }
    settings
}

/// A JSON number that is a whole number within `min..=max` (`3.0` counts,
/// as with JavaScript's `Number.isInteger`).
fn integer_in(value: &Value, min: u32, max: u32) -> Option<u32> {
    let number = value.as_f64()?;
    if number.fract() != 0.0 || number < f64::from(min) || number > f64::from(max) {
        return None;
    }
    // In range and whole, so the conversion is exact.
    Some(number as u32)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn normalized(value: Value) -> AppSettings {
        normalize_settings(&value)
    }

    #[test]
    fn non_objects_give_defaults() {
        for value in [json!(null), json!([]), json!("x"), json!(3), json!(true)] {
            assert_eq!(normalized(value), AppSettings::default());
        }
    }

    #[test]
    fn library_folder_rules() {
        assert_eq!(
            normalized(json!({"libraryFolder": "/Music"}))
                .library_folder
                .as_deref(),
            Some("/Music")
        );
        assert_eq!(
            normalized(json!({"libraryFolder": ""}))
                .library_folder
                .as_deref(),
            Some("")
        );
        assert_eq!(
            normalized(json!({"libraryFolder": null})).library_folder,
            None
        );
        for invalid in [json!(1), json!(false), json!(["/a"]), json!({"a": 1})] {
            assert_eq!(
                normalized(json!({ "libraryFolder": invalid })).library_folder,
                None
            );
        }
    }

    #[test]
    fn theme_rules() {
        assert_eq!(normalized(json!({"theme": "light"})).theme, Theme::Light);
        assert_eq!(normalized(json!({"theme": "dark"})).theme, Theme::Dark);
        assert_eq!(normalized(json!({"theme": "system"})).theme, Theme::System);
        for invalid in [
            json!("Dark"),
            json!("blue"),
            json!(""),
            json!(null),
            json!(1),
        ] {
            assert_eq!(normalized(json!({ "theme": invalid })).theme, Theme::System);
        }
    }

    #[test]
    fn crossfade_rules() {
        let crossfade = |value: Value| {
            normalized(json!({ "crossfadeDurationSeconds": value })).crossfade_duration_seconds
        };
        assert_eq!(crossfade(json!(0)), 0);
        assert_eq!(crossfade(json!(15)), 15);
        assert_eq!(crossfade(json!(7)), 7); // Any whole number in range, not only the menu options.
        assert_eq!(crossfade(json!(8.0)), 8);
        for invalid in [
            json!(-1),
            json!(16),
            json!(2.5),
            json!("5"),
            json!(null),
            json!(true),
        ] {
            assert_eq!(crossfade(invalid), 3, "falls back to the default");
        }
    }

    #[test]
    fn cache_size_rules() {
        let cache = |value: Value| {
            normalized(json!({ "downloadCacheSizeGiB": value })).download_cache_size_gib
        };
        assert_eq!(cache(json!(1)), 1);
        assert_eq!(cache(json!(25)), 25);
        assert_eq!(cache(json!(12.0)), 12);
        for invalid in [
            json!(0),
            json!(26),
            json!(1.5),
            json!("10"),
            json!(null),
            json!(1e10),
        ] {
            assert_eq!(cache(invalid), 5, "falls back to the default");
        }
    }

    #[test]
    fn fields_are_independent_and_unknown_keys_ignored() {
        let settings = normalized(json!({
            "theme": "dark",
            "crossfadeDurationSeconds": 99,
            "downloadCacheSizeGiB": 10,
            "libraryFolder": 7,
            "somethingElse": true,
        }));
        assert_eq!(
            settings,
            AppSettings {
                library_folder: None,
                theme: Theme::Dark,
                crossfade_duration_seconds: 3,
                download_cache_size_gib: 10,
            }
        );
    }

    #[test]
    fn missing_or_corrupt_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_settings(dir.path()), AppSettings::default());
        std::fs::write(dir.path().join(SETTINGS_FILE), "{not json").unwrap();
        assert_eq!(load_settings(dir.path()), AppSettings::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let settings = AppSettings {
            library_folder: Some("/Users/dj/Music".into()),
            theme: Theme::Light,
            crossfade_duration_seconds: 8,
            download_cache_size_gib: 20,
        };
        save_settings(dir.path(), &settings).unwrap();
        assert_eq!(load_settings(dir.path()), settings);
        let text = std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap();
        assert!(text.contains("\"downloadCacheSizeGiB\": 20"));
        assert!(text.ends_with("}\n"));
    }

    #[test]
    fn missing_settings_give_defaults_without_writing() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(load_settings(root.path()), AppSettings::default());
        assert!(!root.path().join(SETTINGS_FILE).exists());
    }
}
