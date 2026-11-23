use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::IgnoredAny;
use std::path::PathBuf;

/// ~/.config/workstation-connect/config.toml
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Phone's Bluetooth identity address (bluetoothctl devices), e.g. "AA:BB:CC:DD:EE:FF".
    pub phone: String,
    /// NetworkManager connection for the phone's Wi-Fi hotspot.
    pub hotspot_connection: String,
    /// Obsolete (Bluetooth tethering is gone): accepted so old configs still load, ignored.
    #[serde(default)]
    bt_connection: Option<IgnoredAny>,
    /// Obsolete (the hotspot is always used): accepted so old configs still load, ignored.
    #[serde(default)]
    hotspot: Option<IgnoredAny>,
    /// Seconds offline before asking the phone while a saved Wi-Fi network is in range (NM's
    /// chance to join it). None in range: the phone is asked right away.
    #[serde(default = "default_offline_secs")]
    pub offline_secs: u64,
    /// Saved Wi-Fi networks never worth leaving the hotspot for (train Wi-Fi with captive portal).
    #[serde(default)]
    pub ignore_ssids: Vec<String>,
    /// KDE Connect device id or name of the phone. Default: first paired smartphone.
    pub kdeconnect_device: Option<String>,
    /// Menu command reading entries on stdin (dmenu mode). Default: walker/fuzzel/wofi/rofi, first found.
    pub menu: Option<Vec<String>>,
    /// Install updates of GitHub main right away (false: waybar marker + menu item only).
    #[serde(default = "yes")]
    pub auto_update: bool,
    /// Folder for files/photos shared from the phone (`workstation-connect setup` applies it), e.g. "~/Pictures/Phone".
    pub share_dir: Option<String>,
}

fn default_offline_secs() -> u64 {
    15
}

fn yes() -> bool {
    true
}

pub fn xdg_config() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
}

/// ~/.local/state/workstation-connect: daemon state that survives restarts.
pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("workstation-connect")
}

pub fn dir() -> PathBuf {
    xdg_config().join("workstation-connect")
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = dir().join("config.toml");
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    /// Obsolete keys present in the file (ignored; worth a warning).
    pub fn obsolete_keys(&self) -> Vec<&'static str> {
        [
            ("bt_connection", self.bt_connection.is_some()),
            ("hotspot", self.hotspot.is_some()),
        ]
        .into_iter()
        .filter_map(|(k, set)| set.then_some(k))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal() {
        let c: Config = toml::from_str(
            r#"phone = "AA:BB:CC:DD:EE:FF"
hotspot_connection = "Pixel 9 hotspot""#,
        )
        .unwrap();
        assert_eq!(c.hotspot_connection, "Pixel 9 hotspot");
        assert_eq!(c.offline_secs, 15);
        assert!(c.ignore_ssids.is_empty());
        assert!(c.auto_update);
        assert!(c.obsolete_keys().is_empty());
    }

    #[test]
    fn accepts_obsolete_bt_keys() {
        let c: Config = toml::from_str(
            r#"phone = "AA:BB:CC:DD:EE:FF"
bt_connection = "Pixel 9 BT"
hotspot = true
hotspot_connection = "Pixel 9 hotspot""#,
        )
        .unwrap();
        assert_eq!(c.hotspot_connection, "Pixel 9 hotspot");
        assert_eq!(c.obsolete_keys(), ["bt_connection", "hotspot"]);
    }

    #[test]
    fn needs_hotspot_connection() {
        let e = toml::from_str::<Config>(
            r#"phone = "AA:BB:CC:DD:EE:FF"
bt_connection = "Pixel 9 BT""#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("hotspot_connection"), "{e}");
    }

    #[test]
    fn still_rejects_unknown_keys() {
        assert!(
            toml::from_str::<Config>(
                r#"phone = "AA:BB:CC:DD:EE:FF"
hotspot_connection = "x"
typo = 1"#,
            )
            .is_err()
        );
    }
}
