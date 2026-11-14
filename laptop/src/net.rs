//! NetworkManager: state over D-Bus (polled), actions via nmcli.

use anyhow::{Context, Result, bail};
use std::time::Duration;
use tokio::process::Command;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    #[zbus(property)]
    fn connectivity(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn active_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn check_connectivity(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager"
)]
trait ActiveConnection {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property, name = "Type")]
    fn kind(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
}

const CONNECTIVITY_UNKNOWN: u32 = 0;
const CONNECTIVITY_FULL: u32 = 4;
const NM_STATE_CONNECTING: u32 = 40;
const NM_STATE_CONNECTED_GLOBAL: u32 = 70;
const ACTIVE_ACTIVATING: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// Full internet (NM connectivity check, or NM's global state if checks are disabled).
    pub online: bool,
    /// NM is busy bringing something up; don't judge connectivity yet.
    pub connecting: bool,
    /// Active NM connections.
    pub active: Vec<ActiveConn>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveConn {
    pub id: String,
    /// NM type, e.g. "802-11-wireless", "bluetooth", "802-3-ethernet"
    pub kind: String,
}

impl Snapshot {
    pub fn is_active(&self, id: &str) -> bool {
        self.active.iter().any(|a| a.id == id)
    }
}

pub struct Nm {
    bus: zbus::Connection,
}

impl Nm {
    pub async fn new() -> Result<Self> {
        Ok(Self {
            bus: zbus::Connection::system().await.context("system bus")?,
        })
    }

    async fn manager(&self) -> Result<NetworkManagerProxy<'_>> {
        Ok(NetworkManagerProxy::builder(&self.bus)
            .cache_properties(CacheProperties::No)
            .build()
            .await?)
    }

    pub async fn snapshot(&self) -> Result<Snapshot> {
        let nm = self.manager().await?;
        let connectivity = nm.connectivity().await?;
        let state = nm.state().await?;
        let mut snap = Snapshot {
            online: connectivity == CONNECTIVITY_FULL
                || (connectivity == CONNECTIVITY_UNKNOWN && state == NM_STATE_CONNECTED_GLOBAL),
            connecting: state == NM_STATE_CONNECTING,
            active: Vec::new(),
        };
        for path in nm.active_connections().await? {
            let Ok(builder) = ActiveConnectionProxy::builder(&self.bus).path(path) else {
                continue;
            };
            // Connections vanish between listing and reading; skip those.
            let Ok(ac) = builder.cache_properties(CacheProperties::No).build().await else {
                continue;
            };
            let (Ok(id), Ok(kind), Ok(st)) = (ac.id().await, ac.kind().await, ac.state().await)
            else {
                continue;
            };
            snap.connecting |= st == ACTIVE_ACTIVATING;
            snap.active.push(ActiveConn { id, kind });
        }
        Ok(snap)
    }

    /// Ask NM to re-run its connectivity check now (it otherwise waits up to 5 min).
    pub async fn recheck(&self) {
        if let Ok(nm) = self.manager().await {
            let _ = nm.check_connectivity().await;
        }
    }
}

async fn nmcli(args: &[&str], timeout: Duration) -> Result<String> {
    let out = tokio::time::timeout(
        timeout,
        Command::new("nmcli").args(args).kill_on_drop(true).output(),
    )
    .await
    .with_context(|| format!("nmcli {} timed out", args.join(" ")))??;
    if !out.status.success() {
        bail!(
            "nmcli {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub async fn up(id: &str, wait_secs: u64) -> Result<()> {
    let wait = wait_secs.to_string();
    nmcli(
        &["--wait", &wait, "connection", "up", "id", id],
        Duration::from_secs(wait_secs + 5),
    )
    .await?;
    Ok(())
}

pub async fn rescan() {
    let _ = nmcli(&["device", "wifi", "rescan"], Duration::from_secs(10)).await;
}

pub async fn down(id: &str) -> Result<()> {
    nmcli(&["connection", "down", "id", id], Duration::from_secs(15)).await?;
    Ok(())
}

/// A saved Wi-Fi connection (not excluded) whose SSID is in range with decent signal.
pub async fn known_wifi_in_range(
    exclude_ids: &[&str],
    exclude_ssids: &[String],
) -> Result<Option<String>> {
    let t = Duration::from_secs(20);
    let mut saved = Vec::new();
    for line in nmcli(&["-t", "-f", "NAME,TYPE", "connection", "show"], t)
        .await?
        .lines()
    {
        let f = split_terse(line);
        if f.len() == 2 && f[1] == "802-11-wireless" && !exclude_ids.contains(&f[0].as_str()) {
            let ssid = nmcli(
                &[
                    "-g",
                    "802-11-wireless.ssid",
                    "connection",
                    "show",
                    "id",
                    &f[0],
                ],
                t,
            )
            .await?;
            saved.push((f[0].clone(), ssid.trim().to_string()));
        }
    }
    let scan = nmcli(
        &[
            "-t",
            "-f",
            "SSID,SIGNAL",
            "device",
            "wifi",
            "list",
            "--rescan",
            "yes",
        ],
        t,
    )
    .await?;
    Ok(pick_known(&saved, &scan, exclude_ssids))
}

const MIN_SIGNAL: u32 = 40;

fn pick_known(saved: &[(String, String)], scan: &str, exclude_ssids: &[String]) -> Option<String> {
    let visible: Vec<(String, u32)> = scan
        .lines()
        .filter_map(|l| {
            let f = split_terse(l);
            Some((f.first()?.clone(), f.get(1)?.parse().ok()?))
        })
        .collect();
    saved
        .iter()
        .filter(|(_, ssid)| !ssid.is_empty() && !exclude_ssids.contains(ssid))
        .filter_map(|(id, ssid)| {
            let best = visible
                .iter()
                .filter(|(s, _)| s == ssid)
                .map(|(_, sig)| *sig)
                .max()?;
            (best >= MIN_SIGNAL).then_some((id.clone(), best))
        })
        .max_by_key(|(_, sig)| *sig)
        .map(|(id, _)| id)
}

/// Split one `nmcli -t` line: fields separated by ':', literal ':' and '\' escaped with '\'.
fn split_terse(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => fields.last_mut().unwrap().extend(chars.next()),
            ':' => fields.push(String::new()),
            c => fields.last_mut().unwrap().push(c),
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_escaped_fields() {
        assert_eq!(
            split_terse(r"my\:wifi:802-11-wireless"),
            ["my:wifi", "802-11-wireless"]
        );
        assert_eq!(split_terse(r"a\\b:"), ["a\\b", ""]);
    }

    #[test]
    fn picks_strongest_known_not_ignored() {
        let saved = vec![
            ("Home".to_string(), "home".to_string()),
            ("eduroam".to_string(), "eduroam".to_string()),
            ("Train".to_string(), "WiFi in de trein".to_string()),
        ];
        let scan = "home:30\neduroam:70\nWiFi in de trein:90\nother:99\n";
        assert_eq!(
            pick_known(&saved, scan, &["WiFi in de trein".into()]),
            Some("eduroam".into())
        );
        assert_eq!(pick_known(&saved, "home:20\n", &[]), None);
    }
}
