use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::PathBuf;

/// ~/.config/workstation-connect/config.toml
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Phone's Bluetooth identity address (bluetoothctl devices), e.g. "AA:BB:CC:DD:EE:FF".
    pub phone: String,
    /// NetworkManager connection for the phone's Bluetooth tethering (PAN).
    pub bt_connection: String,
    /// After Bluetooth tethering, also bring up the phone's Wi-Fi hotspot and move there (faster,
    /// costs phone battery). Default off: Bluetooth tethering only.
    #[serde(default)]
    pub hotspot: bool,
    /// NetworkManager connection for the phone's Wi-Fi hotspot. Needed with `hotspot = true`.
    pub hotspot_connection: Option<String>,
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
    /// Folder for files/photos shared from the phone (`wsconnect setup` applies it), e.g. "~/Pictures/Phone".
    pub share_dir: Option<String>,
}

fn default_offline_secs() -> u64 {
    15
}

pub fn xdg_config() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
}

pub fn dir() -> PathBuf {
    xdg_config().join("workstation-connect")
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = dir().join("config.toml");
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let cfg: Self =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        if cfg.hotspot && cfg.hotspot_connection.is_none() {
            bail!(
                "{}: hotspot = true needs hotspot_connection",
                path.display()
            );
        }
        Ok(cfg)
    }

    /// The hotspot connection, if configured (also when `hotspot` is off: a hotspot joined by
    /// hand still counts as being on the phone).
    pub fn hotspot_id(&self) -> Option<&str> {
        self.hotspot_connection.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal() {
        let c: Config = toml::from_str(
            r#"phone = "AA:BB:CC:DD:EE:FF"
bt_connection = "Pixel 9 Network""#,
        )
        .unwrap();
        assert!(!c.hotspot && c.hotspot_id().is_none());
        assert_eq!(c.offline_secs, 15);
        assert!(c.ignore_ssids.is_empty());
    }
}
