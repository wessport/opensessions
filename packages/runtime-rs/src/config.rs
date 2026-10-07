use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::protocol::SessionFilterMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarPosition {
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OpensessionsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mux: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transparent_background: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_width: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_position: Option<SidebarPosition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keybinding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_panel_height: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_filter: Option<SessionFilterMode>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_auto_hibernate"
    )]
    pub auto_hibernate: Option<AutoHibernateConfig>,
}

/// Default idle age before a live idle agent process is hibernated.
pub const DEFAULT_AUTO_HIBERNATE_IDLE_AFTER_MS: u64 = 6 * 60 * 60 * 1000;

/// `autoHibernate` in `config.json`. Missing fields use defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoHibernateConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_after_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoHibernateSettings {
    pub enabled: bool,
    pub idle_after_ms: u64,
}

impl Default for AutoHibernateSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_after_ms: DEFAULT_AUTO_HIBERNATE_IDLE_AFTER_MS,
        }
    }
}

impl OpensessionsConfig {
    /// Auto-hibernation is enabled by default; only an explicit
    /// `"enabled": false` turns it off.
    pub fn auto_hibernate_settings(&self) -> AutoHibernateSettings {
        let config = self.auto_hibernate.unwrap_or_default();
        AutoHibernateSettings {
            enabled: config.enabled != Some(false),
            idle_after_ms: config
                .idle_after_ms
                .filter(|idle_after_ms| *idle_after_ms > 0)
                .unwrap_or(DEFAULT_AUTO_HIBERNATE_IDLE_AFTER_MS),
        }
    }
}

/// Malformed `autoHibernate` values fall back to defaults instead of
/// invalidating the rest of the user's config.
fn deserialize_auto_hibernate<'de, D>(
    deserializer: D,
) -> Result<Option<AutoHibernateConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    let Value::Object(map) = value else {
        return Ok(None);
    };
    Ok(Some(AutoHibernateConfig {
        enabled: map.get("enabled").and_then(Value::as_bool),
        idle_after_ms: map.get("idleAfterMs").and_then(|value| {
            value.as_u64().or_else(|| {
                value
                    .as_f64()
                    .filter(|ms| ms.is_finite() && *ms > 0.0)
                    .map(|ms| ms as u64)
            })
        }),
    }))
}

pub fn config_path_from_home(home: &Path) -> PathBuf {
    home.join(".config")
        .join("opensessions")
        .join("config.json")
}

pub fn load_config_from_home(home: &Path) -> OpensessionsConfig {
    let path = config_path_from_home(home);
    let Ok(raw) = fs::read_to_string(path) else {
        return OpensessionsConfig::default();
    };

    let Ok(mut config) = serde_json::from_str::<OpensessionsConfig>(&raw) else {
        return OpensessionsConfig::default();
    };

    if config.plugins.is_empty() {
        config.plugins = Vec::new();
    }

    config
}

/// Merges `updates` into the config file and writes it atomically.
///
/// A config file that exists but is not a JSON object is left untouched and
/// reported as an error: overwriting it would silently discard the user's
/// settings over a typo.
pub fn save_config_to_home(home: &Path, updates: OpensessionsConfig) -> io::Result<()> {
    let path = config_path_from_home(home);
    let mut merged = match fs::read_to_string(&path) {
        Ok(raw) if raw.trim().is_empty() => serde_json::json!({ "plugins": [] }),
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(map)) => Value::Object(map),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "refusing to overwrite {}: not a JSON object",
                        path.display()
                    ),
                ));
            }
            Err(error) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "refusing to overwrite unparseable {}: {error}",
                        path.display()
                    ),
                ));
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            serde_json::json!({ "plugins": [] })
        }
        Err(error) => return Err(error),
    };

    merge_value(&mut merged, Value::Object(update_map(updates)));

    let encoded = serde_json::to_string_pretty(&merged).map_err(io::Error::other)?;
    write_file_atomically(&path, &format!("{encoded}\n"))
}

/// Writes `contents` to a temporary file beside `path` and renames it into
/// place, so a crash or a concurrent reader never sees a partial file.
pub(crate) fn write_file_atomically(path: &Path, contents: &str) -> io::Result<()> {
    static NEXT_TEMPORARY: AtomicUsize = AtomicUsize::new(0);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let temporary = parent.join(format!(
        ".{}.tmp.{}.{}",
        file_name.to_string_lossy(),
        std::process::id(),
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn update_map(updates: OpensessionsConfig) -> Map<String, Value> {
    let mut map = Map::new();

    insert_option(&mut map, "mux", updates.mux);
    insert_option(&mut map, "port", updates.port);
    if !updates.plugins.is_empty() {
        map.insert(
            "plugins".to_string(),
            serde_json::to_value(updates.plugins).expect("plugins serialize"),
        );
    }
    insert_option(&mut map, "theme", updates.theme);
    insert_option(
        &mut map,
        "transparentBackground",
        updates.transparent_background,
    );
    insert_option(&mut map, "sidebarWidth", updates.sidebar_width);
    insert_option(&mut map, "sidebarPosition", updates.sidebar_position);
    insert_option(&mut map, "keybinding", updates.keybinding);
    insert_option(&mut map, "detailPanelHeight", updates.detail_panel_height);
    insert_option(&mut map, "sessionFilter", updates.session_filter);
    insert_option(&mut map, "autoHibernate", updates.auto_hibernate);

    map
}

fn insert_option<T: Serialize>(map: &mut Map<String, Value>, key: &str, value: Option<T>) {
    if let Some(value) = value {
        map.insert(
            key.to_string(),
            serde_json::to_value(value).expect("config value serialize"),
        );
    }
}

fn merge_value(dst: &mut Value, src: Value) {
    match (dst, src) {
        (Value::Object(dst), Value::Object(src)) => {
            for (key, value) in src {
                match dst.get_mut(&key) {
                    Some(existing) => merge_value(existing, value),
                    None => {
                        dst.insert(key, value);
                    }
                }
            }
        }
        (dst, src) => *dst = src,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> PathBuf {
        static NEXT_HOME: AtomicUsize = AtomicUsize::new(0);
        let home = std::env::temp_dir().join(format!(
            "opensessions-config-{name}-{}-{}",
            std::process::id(),
            NEXT_HOME.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&home);
        home
    }

    #[test]
    fn saving_never_overwrites_an_unparseable_config() {
        let home = temp_home("unparseable");
        let path = config_path_from_home(&home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let broken = "{ \"sidebarWidth\": 40, \"theme\": \"nord\", }\n";
        fs::write(&path, broken).unwrap();

        let result = save_config_to_home(
            &home,
            OpensessionsConfig {
                sidebar_width: Some(52),
                ..OpensessionsConfig::default()
            },
        );

        let kept = fs::read_to_string(&path).unwrap();
        fs::remove_dir_all(&home).unwrap();
        assert_eq!(
            result.map_err(|error| error.kind()),
            Err(io::ErrorKind::InvalidData)
        );
        assert_eq!(kept, broken);
    }

    #[test]
    fn saving_merges_into_the_existing_config_without_leaving_temp_files() {
        let home = temp_home("merge");
        let path = config_path_from_home(&home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"theme":"nord","plugins":["x"]}"#).unwrap();

        save_config_to_home(
            &home,
            OpensessionsConfig {
                sidebar_width: Some(52),
                ..OpensessionsConfig::default()
            },
        )
        .expect("save config");

        let saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let entries = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        fs::remove_dir_all(&home).unwrap();
        assert_eq!(saved["theme"], "nord");
        assert_eq!(saved["sidebarWidth"], 52);
        assert_eq!(entries, vec!["config.json".to_string()]);
    }

    fn parse(raw: &str) -> OpensessionsConfig {
        serde_json::from_str(raw).expect("config parses")
    }

    #[test]
    fn auto_hibernate_is_enabled_for_six_hours_when_missing() {
        assert_eq!(
            parse("{}").auto_hibernate_settings(),
            AutoHibernateSettings {
                enabled: true,
                idle_after_ms: 21_600_000,
            }
        );
    }

    #[test]
    fn auto_hibernate_honours_idle_override() {
        let config = parse(r#"{"autoHibernate":{"idleAfterMs":60000}}"#);
        assert_eq!(
            config.auto_hibernate_settings(),
            AutoHibernateSettings {
                enabled: true,
                idle_after_ms: 60_000,
            }
        );
    }

    #[test]
    fn auto_hibernate_can_be_disabled() {
        let config = parse(r#"{"autoHibernate":{"enabled":false}}"#);
        assert!(!config.auto_hibernate_settings().enabled);
    }

    #[test]
    fn malformed_auto_hibernate_uses_defaults_without_dropping_other_settings() {
        let config =
            parse(r#"{"sidebarWidth":40,"autoHibernate":{"enabled":"no","idleAfterMs":-5}}"#);
        assert_eq!(config.sidebar_width, Some(40));
        assert_eq!(
            config.auto_hibernate_settings(),
            AutoHibernateSettings::default()
        );

        let config = parse(r#"{"autoHibernate":true,"sidebarWidth":40}"#);
        assert_eq!(config.sidebar_width, Some(40));
        assert_eq!(
            config.auto_hibernate_settings(),
            AutoHibernateSettings::default()
        );
    }
}
