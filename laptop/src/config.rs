use anyhow::{Context, Result};
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
    /// NetworkManager connection for the phone's Wi-Fi hotspot.
    pub hotspot_connection: String,
    /// Seconds without internet before asking the phone.
    #[serde(default = "default_offline_secs")]
    pub offline_secs: u64,
    /// Saved Wi-Fi networks never worth leaving the hotspot for (train Wi-Fi with captive portal).
    #[serde(default)]
    pub ignore_ssids: Vec<String>,
}

fn default_offline_secs() -> u64 {
    10
}

pub fn dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("workstation-connect")
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = dir().join("config.toml");
        let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal() {
        let c: Config = toml::from_str(
            r#"phone = "AA:BB:CC:DD:EE:FF"
bt_connection = "Pixel 9 Network"
hotspot_connection = "Pixel 9""#,
        )
        .unwrap();
        assert_eq!(c.offline_secs, 10);
        assert!(c.ignore_ssids.is_empty());
    }
}
