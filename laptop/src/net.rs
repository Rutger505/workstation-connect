//! NetworkManager: state over D-Bus (polled; Wi-Fi device changes as events), actions via nmcli.

use anyhow::{Context, Result, bail};
use futures::{Stream, StreamExt};
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
    #[zbus(property)]
    fn primary_connection(&self) -> zbus::Result<OwnedObjectPath>;
    fn check_connectivity(&self) -> zbus::Result<u32>;
    fn get_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Device",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Device {
    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn active_connection(&self) -> zbus::Result<OwnedObjectPath>;
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
const DEVICE_TYPE_WIFI: u32 = 2;

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

    /// Fires on every (re)connect: primary connection changed or connectivity became full.
    /// Includes the initial values once.
    pub async fn reconnects(&self) -> Result<impl Stream<Item = ()> + '_> {
        let nm = NetworkManagerProxy::new(&self.bus).await?;
        let primary = nm.receive_primary_connection_changed().await.map(|_| ());
        let full = nm
            .receive_connectivity_changed()
            .await
            .filter_map(
                |c| async move { (c.get().await.ok()? == CONNECTIVITY_FULL).then_some(()) },
            );
        Ok(futures::stream::select(primary, full))
    }

    /// The Wi-Fi device's active connection as `(object path, id)`, now and on every change.
    /// Path "/" (id empty) = not connected.
    pub async fn wifi_connections(&self) -> Result<impl Stream<Item = (String, String)> + '_> {
        let nm = self.manager().await?;
        let mut wifi = None;
        for path in nm.get_devices().await? {
            let dev = DeviceProxy::builder(&self.bus).path(path)?.build().await?;
            if dev.device_type().await? == DEVICE_TYPE_WIFI {
                wifi = Some(dev);
                break;
            }
        }
        let dev = wifi.context("no Wi-Fi device")?;
        let changes = dev.receive_active_connection_changed().await;
        Ok(changes.then(move |c| async move {
            let Ok(path) = c.get().await else {
                return ("/".to_string(), String::new());
            };
            let id = self.active_id(&path).await.unwrap_or_else(|| "?".into());
            (path.to_string(), id)
        }))
    }

    async fn active_id(&self, path: &OwnedObjectPath) -> Option<String> {
        if path.as_str() == "/" {
            return Some(String::new());
        }
        let ac = ActiveConnectionProxy::builder(&self.bus)
            .path(path.clone())
            .ok()?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .ok()?;
        ac.id().await.ok()
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

/// Marks the hotspot connection hidden: NetworkManager then probes for its SSID instead of
/// waiting until a scan lists it (after resume the scan cache is stale: seconds lost).
pub async fn ensure_hidden(id: &str) -> Result<()> {
    let out = nmcli(
        &[
            "-g",
            "802-11-wireless.hidden",
            "connection",
            "show",
            "id",
            id,
        ],
        Duration::from_secs(10),
    )
    .await?;
    if out.trim() != "yes" {
        nmcli(
            &[
                "connection",
                "modify",
                "id",
                id,
                "802-11-wireless.hidden",
                "yes",
            ],
            Duration::from_secs(10),
        )
        .await?;
        tracing::info!("{id}: set 802-11-wireless.hidden yes (faster connect)");
    }
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
    let saved = saved_wifi(exclude_ids).await?;
    let scan = scan().await?;
    Ok(pick_known(&saved, &scan, exclude_ssids, MIN_SIGNAL))
}

/// Any saved Wi-Fi network (not excluded) in range at all, i.e. NM may still join one by itself.
pub async fn known_wifi_visible(exclude_ids: &[&str], exclude_ssids: &[String]) -> Result<bool> {
    let saved = saved_wifi(exclude_ids).await?;
    let scan = scan().await?;
    Ok(pick_known(&saved, &scan, exclude_ssids, 0).is_some())
}

const NMCLI_SLOW: Duration = Duration::from_secs(20);

/// (connection id, SSID) of saved Wi-Fi connections.
async fn saved_wifi(exclude_ids: &[&str]) -> Result<Vec<(String, String)>> {
    let mut saved = Vec::new();
    for line in nmcli(&["-t", "-f", "NAME,TYPE", "connection", "show"], NMCLI_SLOW)
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
                NMCLI_SLOW,
            )
            .await?;
            saved.push((f[0].clone(), ssid.trim().to_string()));
        }
    }
    Ok(saved)
}

/// Fresh scan (blocks until done), `nmcli -t` SSID:SIGNAL lines.
async fn scan() -> Result<String> {
    nmcli(
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
        NMCLI_SLOW,
    )
    .await
}

const MIN_SIGNAL: u32 = 40;

fn pick_known(
    saved: &[(String, String)],
    scan: &str,
    exclude_ssids: &[String],
    min_signal: u32,
) -> Option<String> {
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
            (best >= min_signal).then_some((id.clone(), best))
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
            pick_known(&saved, scan, &["WiFi in de trein".into()], MIN_SIGNAL),
            Some("eduroam".into())
        );
        assert_eq!(pick_known(&saved, "home:20\n", &[], MIN_SIGNAL), None);
        // "visible at all": weak counts, ignored never does.
        assert!(pick_known(&saved, "home:20\n", &[], 0).is_some());
        let train = ["WiFi in de trein".to_string()];
        assert!(pick_known(&saved, "WiFi in de trein:90\nother:99\n", &train, 0).is_none());
    }
}
