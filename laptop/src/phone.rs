//! BLE link to the phone: keeps a GATT connection open, streams its state, sends commands.

use crate::proto::{self, Cmd, PhoneState};
use anyhow::{Context, Result, anyhow, bail};
use bluer::agent::{Agent, RequestConfirmation};
use bluer::gatt::WriteOp;
use bluer::gatt::remote::{Characteristic, CharacteristicWriteRequest};
use bluer::{
    Adapter, Address, DeviceEvent, DeviceProperty, DiscoveryFilter, DiscoveryTransport, Session,
};
use futures::{FutureExt, StreamExt, pin_mut};
use serde::Serialize;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{sleep, timeout};
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Link {
    pub connected: bool,
    pub state: Option<PhoneState>,
    /// LE link up but our GATT service missing on several tries in a row: app's service stopped,
    /// or BlueZ serves a stale GATT cache of the phone.
    pub service_missing: bool,
}

/// Any single D-Bus call to bluetoothd. bluer's own is 120 s, zbus has none: a bluetoothd that
/// never answers (seen after resume from hibernate) hung the daemon silently.
const CALL: Duration = Duration::from_secs(30);
/// Everything from opening the session to "phone connected", incl. a first-run LE pairing.
const SETUP: Duration = Duration::from_secs(180);
/// Consecutive "service missing" tries before the tray warns.
const MISSING_WARN: u32 = 3;

/// Connected over LE, but our service isn't among the phone's GATT services.
#[derive(Debug)]
struct ServiceMissing {
    le_connected: bool,
    msg: String,
}

impl std::fmt::Display for ServiceMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for ServiceMissing {}

type Request = (Cmd, oneshot::Sender<Result<()>>);

#[derive(Clone)]
pub struct Phone {
    cmds: mpsc::Sender<Request>,
    link: watch::Receiver<Link>,
}

impl Phone {
    pub fn spawn(addr: Address) -> Self {
        let (cmds, rx) = mpsc::channel(8);
        let (tx, link) = watch::channel(Link::default());
        tokio::spawn(run(addr, rx, tx));
        Self { cmds, link }
    }

    pub fn link(&self) -> Link {
        *self.link.borrow()
    }

    pub fn watch(&self) -> watch::Receiver<Link> {
        self.link.clone()
    }

    /// Write a command; fails if the phone isn't connected within `wait`.
    pub async fn send(&self, cmd: Cmd, wait: Duration) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send((cmd, tx))
            .await
            .map_err(|_| anyhow!("phone task gone"))?;
        timeout(wait, rx)
            .await
            .with_context(|| format!("{cmd:?}: phone not reachable"))??
    }

    /// Wait until the phone reports a state matching `f`.
    pub async fn wait_for(&self, wait: Duration, f: impl Fn(&PhoneState) -> bool) -> bool {
        let mut rx = self.link.clone();
        timeout(wait, rx.wait_for(|l| l.state.as_ref().is_some_and(&f)))
            .await
            .is_ok_and(|r| r.is_ok())
    }
}

async fn run(addr: Address, mut cmds: mpsc::Receiver<Request>, link: watch::Sender<Link>) {
    let mut backoff = Duration::from_secs(2);
    let mut missing = 0;
    loop {
        let started = std::time::Instant::now();
        let res = async {
            let (session, adapter) = timeout(CALL, async {
                let session = bluer::Session::new().await?;
                let adapter = session.default_adapter().await?;
                anyhow::Ok((session, adapter))
            })
            .await
            .context("bluetoothd not answering")??;
            if !timeout(CALL, adapter.is_powered())
                .await
                .context("bluetoothd not answering")??
            {
                bail!("bluetooth adapter is off");
            }
            connect_and_serve(&session, &adapter, addr, &mut cmds, &link).await
        }
        .await;
        match &res {
            Err(e)
                if e.downcast_ref::<ServiceMissing>()
                    .is_some_and(|m| m.le_connected) =>
            {
                missing += 1
            }
            _ => missing = 0,
        }
        link.send_replace(Link {
            service_missing: missing >= MISSING_WARN,
            ..Default::default()
        });
        if let Err(e) = res {
            warn!("phone link: {e:#}");
        }
        // Commands queued while offline would be stale; fail them now.
        while let Ok((cmd, reply)) = cmds.try_recv() {
            let _ = reply.send(Err(anyhow!("{cmd:?}: phone disconnected")));
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(2); // was a real session, not a failing retry
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn connect_and_serve(
    session: &Session,
    adapter: &Adapter,
    addr: Address,
    cmds: &mut mpsc::Receiver<Request>,
    link: &watch::Sender<Link>,
) -> Result<()> {
    let (device, le, control, state, initial) = timeout(SETUP, connect(session, adapter, addr))
        .await
        .with_context(|| {
            format!(
                "no connection after {}s: bluetoothd stuck?",
                SETUP.as_secs()
            )
        })??;
    link.send_replace(Link {
        connected: true,
        state: PhoneState::parse(&initial),
        service_missing: false,
    });
    info!("phone connected: {:?}", link.borrow().state);

    let notify = timeout(CALL, state.notify())
        .await
        .context("subscribe state: bluetoothd not answering")?
        .context("subscribe state")?;
    let events = timeout(CALL, device.events())
        .await
        .context("device events: bluetoothd not answering")??;
    pin_mut!(notify, events);
    let mut le_check = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = le_check.tick() => {
                // Device-level Connected stays true while BR/EDR is up; the LE link is what we use.
                // A failing/hanging read means bluetoothd lost the bearer or got stuck: reconnect.
                if !timeout(CALL, le.connected()).await.context("LE state: bluetoothd not answering")?.context("LE state")? {
                    bail!("LE link lost");
                }
            }
            v = notify.next() => {
                let v = v.ok_or_else(|| anyhow!("notifications ended"))?;
                link.send_modify(|l| l.state = PhoneState::parse(&v).or(l.state));
            }
            ev = events.next() => match ev {
                Some(DeviceEvent::PropertyChanged(DeviceProperty::Connected(false))) | None => bail!("disconnected"),
                _ => {}
            },
            Some((cmd, reply)) = cmds.recv() => {
                if reply.is_closed() { continue; } // caller gave up
                let req = CharacteristicWriteRequest { op_type: WriteOp::Request, ..Default::default() };
                let res = match timeout(CALL, control.write_ext(&[cmd as u8], &req)).await {
                    Ok(r) => r.map_err(|e| anyhow!("write {cmd:?}: {e}")),
                    Err(_) => Err(anyhow!("write {cmd:?}: bluetoothd not answering")),
                };
                info!("sent {cmd:?}: {res:?}");
                let failed = res.is_err();
                let _ = reply.send(res);
                if failed { bail!("write failed"); }
            }
        }
    }
}

/// LE-connect the phone and find our service; returns its characteristics + initial state.
async fn connect(
    session: &Session,
    adapter: &Adapter,
    addr: Address,
) -> Result<(
    bluer::Device,
    LeBearer,
    Characteristic,
    Characteristic,
    Vec<u8>,
)> {
    if !adapter.device(addr)?.is_paired().await.unwrap_or(false) {
        bail!("{addr} is not paired: bluetoothctl pair {addr} && bluetoothctl trust {addr}");
    }
    let (device, le) = match find_le_device(adapter, addr).await? {
        Some(found) => found,
        None => pair_le(session, adapter, addr).await?,
    };
    if !le.connected().await? {
        // Kernels before 7.4 (397464709a20) connect from a passive scan filtered on the accept
        // list, which drops the phone's private address when the controller can't resolve it
        // itself. Discovery scans unfiltered and the kernel resolves the address with the IRK.
        let discovery = le_discovery(adapter).await?;
        let connect = le.connect();
        pin_mut!(discovery, connect);
        timeout(Duration::from_secs(30), async {
            loop {
                tokio::select! {
                    r = &mut connect => return r,
                    _ = discovery.next() => {}
                }
            }
        })
        .await
        .context("LE connect timed out (app running? Bluetooth on? in range?)")??;
    }
    let found = timeout(Duration::from_secs(15), async {
        loop {
            if let Some(c) = find_chars(&device).await? {
                return Ok::<_, anyhow::Error>(c);
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
    let (control, state) = match found {
        Ok(c) => c?,
        Err(_) => return Err(service_missing(&device, &le).await.into()),
    };
    let initial = state.read().await.context("read state (bonded over LE?)")?;
    Ok((device, le, control, state, initial))
}

/// The phone's BlueZ object that is bonded over LE. A BR/EDR-only bond isn't: BlueZ then lacks
/// the phone's IRK, can't resolve its private LE address and never connects our GATT service.
/// After pairing over LE, BlueZ keeps the LE object under the private address's path (with
/// Address set to the phone's) next to the BR/EDR one until bluetoothd restarts, so match on the
/// Address property, not the path.
async fn find_le_device(
    adapter: &Adapter,
    addr: Address,
) -> Result<Option<(bluer::Device, LeBearer)>> {
    let bus = zbus::connection::Builder::system()?
        .method_timeout(CALL)
        .build()
        .await?;
    let objects = zbus::fdo::ObjectManagerProxy::builder(&bus)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?
        .get_managed_objects()
        .await?;
    let prefix = format!("/org/bluez/{}/dev_", adapter.name());
    for (path, ifaces) in objects {
        let Some(path_addr) = path.as_str().strip_prefix(&prefix) else {
            continue;
        };
        let iface = |name: &str| {
            ifaces
                .iter()
                .find(|(n, _)| n.as_str() == name)
                .map(|(_, p)| p)
        };
        let (Some(dev), Some(le)) = (iface("org.bluez.Device1"), iface("org.bluez.Bearer.LE1"))
        else {
            continue;
        };
        let address = dev
            .get("Address")
            .and_then(|v| v.downcast_ref::<&str>().ok())
            .and_then(|s| s.parse::<Address>().ok());
        if address != Some(addr) {
            continue;
        }
        let Some(bonded) = le.get("Bonded") else {
            bail!(
                "BlueZ hides the LE bearer API (experimental): set `Experimental = true` under \
                 [General] in /etc/bluetooth/main.conf, then `sudo systemctl restart bluetooth`"
            );
        };
        if !bonded.downcast_ref::<bool>().unwrap_or(false) {
            continue;
        }
        let path_addr: Address = path_addr.replace('_', ":").parse()?;
        let le = LeBearer::new(&bus, path.as_str().to_owned()).await?;
        return Ok(Some((adapter.device(path_addr)?, le)));
    }
    Ok(None)
}

/// One-time LE bond: pair whoever advertises our service, keep it only if the identity address it
/// hands over during pairing is the phone's.
async fn pair_le(
    session: &Session,
    adapter: &Adapter,
    addr: Address,
) -> Result<(bluer::Device, LeBearer)> {
    let rpa = find_advertiser(adapter, addr).await?;
    warn!("no LE bond with {addr} yet: pairing over LE via {rpa}, accept the request on the phone");
    // Not the default agent: it only answers for pairings this session starts.
    let _agent = session
        .register_agent(Agent {
            request_confirmation: Some(Box::new(|req: RequestConfirmation| {
                info!(
                    "LE pairing code {:06} (should match the phone)",
                    req.passkey
                );
                async { Ok(()) }.boxed()
            })),
            ..Default::default()
        })
        .await?;
    let paired = timeout(Duration::from_secs(60), adapter.device(rpa)?.pair()).await;
    if let Some(found) = find_le_device(adapter, addr).await? {
        info!("LE bond with {addr} created");
        return Ok(found);
    }
    let _ = adapter.remove_device(rpa).await;
    paired
        .context("LE pairing timed out (not accepted on the phone?)")?
        .context("LE pairing")?;
    bail!("{rpa} advertised our service but paired as someone else than {addr}: removed it")
}

/// LE discovery, running while the stream is alive.
async fn le_discovery(
    adapter: &Adapter,
) -> Result<impl futures::Stream<Item = bluer::AdapterEvent>> {
    adapter
        .set_discovery_filter(DiscoveryFilter {
            transport: DiscoveryTransport::Le,
            ..Default::default()
        })
        .await?;
    Ok(adapter.discover_devices().await?)
}

/// Discover the not yet bonded private address the phone advertises our service from.
async fn find_advertiser(adapter: &Adapter, addr: Address) -> Result<Address> {
    let discovery = le_discovery(adapter).await?;
    pin_mut!(discovery);
    timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                _ = discovery.next() => {}
                _ = sleep(Duration::from_millis(500)) => {}
            }
            for a in adapter.device_addresses().await? {
                if a != addr && advertises_service(&adapter.device(a)?).await {
                    return Ok(a);
                }
            }
        }
    })
    .await
    .context("phone not advertising over LE (app running? Bluetooth on? in range?)")?
}

/// Seen in this discovery (RSSI is cleared when one ends), unpaired, with our service UUID.
async fn advertises_service(device: &bluer::Device) -> bool {
    let seen = device.rssi().await.ok().flatten().is_some();
    let unpaired = !device.is_paired().await.unwrap_or(true);
    let ours = device
        .uuids()
        .await
        .ok()
        .flatten()
        .is_some_and(|u| u.contains(&proto::SERVICE));
    seen && unpaired && ours
}

async fn find_chars(device: &bluer::Device) -> Result<Option<(Characteristic, Characteristic)>> {
    for service in device.services().await? {
        if service.uuid().await? != proto::SERVICE {
            continue;
        }
        let (mut control, mut state) = (None, None);
        for c in service.characteristics().await? {
            match c.uuid().await? {
                u if u == proto::CONTROL => control = Some(c),
                u if u == proto::STATE => state = Some(c),
                _ => {}
            }
        }
        return Ok(Some((
            control.context("no control characteristic")?,
            state.context("no state characteristic")?,
        )));
    }
    Ok(None)
}

async fn service_missing(device: &bluer::Device, le: &LeBearer) -> ServiceMissing {
    let mut seen = Vec::new();
    for s in device.services().await.unwrap_or_default() {
        if let Ok(u) = s.uuid().await {
            seen.push(u.to_string());
        }
    }
    let le_up = le.connected().await.ok();
    let msg = format!(
        "workstation-connect service not found on phone (LE connected: {}, GATT services: [{}]); \
         app outdated or service stopped? Stale BlueZ GATT cache: `Cache = no` under [GATT] in \
         /etc/bluetooth/main.conf (wsconnect setup checks it)",
        le_up.map_or("?".into(), |c| c.to_string()),
        seen.join(", ")
    );
    ServiceMissing {
        le_connected: le_up == Some(true),
        msg,
    }
}

/// org.bluez.Bearer.LE1: the LE link's own state/connect, separate from BR/EDR.
struct LeBearer(zbus::Proxy<'static>);

impl LeBearer {
    async fn new(bus: &zbus::Connection, path: String) -> Result<Self> {
        let proxy = zbus::proxy::Builder::new(bus)
            .destination("org.bluez")?
            .path(path)?
            .interface("org.bluez.Bearer.LE1")?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        Ok(Self(proxy))
    }

    async fn connected(&self) -> zbus::Result<bool> {
        self.0.get_property("Connected").await
    }

    async fn connect(&self) -> Result<()> {
        match self.0.call::<_, _, ()>("Connect", &()).await {
            Err(e) if is_error(&e, "AlreadyConnected") => Ok(()),
            // A connect we timed out on (or a previous daemon run) is still pending in BlueZ, and
            // 5.87's Disconnect can't abort it: wait for it to land instead.
            Err(e) if is_error(&e, "InProgress") => {
                while !self.connected().await? {
                    sleep(Duration::from_millis(500)).await;
                }
                Ok(())
            }
            r => Ok(r.context("LE connect")?),
        }
    }
}

fn is_error(e: &zbus::Error, name: &str) -> bool {
    matches!(e, zbus::Error::MethodError(n, _, _) if n.as_str().strip_prefix("org.bluez.Error.") == Some(name))
}
