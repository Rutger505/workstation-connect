//! BLE link to the phone: keeps a GATT connection open, streams its state, sends commands.

use crate::proto::{self, Cmd, PhoneState};
use anyhow::{Context, Result, anyhow, bail};
use bluer::agent::{Agent, RequestConfirmation};
use bluer::gatt::WriteOp;
use bluer::gatt::remote::{Characteristic, CharacteristicWriteRequest};
use bluer::{Adapter, Address, DiscoveryFilter, DiscoveryTransport, Session};
use futures::{FutureExt, StreamExt, pin_mut};
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, mpsc, oneshot, watch};
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
/// GATT service discovery after our own LE connect.
const DISCOVERY: Duration = Duration::from_secs(30);
/// Same on an LE link we found already up: BlueZ doesn't rerun discovery on an existing link,
/// so ServicesResolved can stay false forever there. Drop it and connect fresh instead.
const ADOPTED_DISCOVERY: Duration = Duration::from_secs(8);

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
    kick: Arc<Notify>,
}

impl Phone {
    pub fn spawn(addr: Address) -> Self {
        let (cmds, rx) = mpsc::channel(8);
        let (tx, link) = watch::channel(Link::default());
        let kick = Arc::new(Notify::new());
        tokio::spawn(run(addr, rx, tx, kick.clone()));
        Self { cmds, link, kick }
    }

    /// Reconnect now instead of finishing the retry backoff (after resume: the link died in the
    /// suspend and tokio timers stand still while suspended, so a 30 s backoff is still 30 s).
    pub fn kick(&self) {
        self.kick.notify_one();
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

async fn run(
    addr: Address,
    mut cmds: mpsc::Receiver<Request>,
    link: watch::Sender<Link>,
    kick: Arc<Notify>,
) {
    let mut backoff = Duration::from_secs(2);
    // After resume the adapter takes a few seconds to come back ("target object ... not
    // present"): retry every second meanwhile instead of backing off.
    let mut fast_until = std::time::Instant::now();
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
        let fast = std::time::Instant::now() < fast_until;
        tokio::select! {
            _ = sleep(if fast { Duration::from_secs(1) } else { backoff }) => if !fast {
                backoff = (backoff * 2).min(Duration::from_secs(30));
            },
            _ = kick.notified() => {
                info!("phone link: reconnecting now (resume)");
                backoff = Duration::from_secs(2);
                fast_until = std::time::Instant::now() + Duration::from_secs(20);
            }
        }
    }
}

async fn connect_and_serve(
    session: &Session,
    adapter: &Adapter,
    addr: Address,
    cmds: &mut mpsc::Receiver<Request>,
    link: &watch::Sender<Link>,
) -> Result<()> {
    let (le, control, state) = timeout(SETUP, connect(session, adapter, addr))
        .await
        .with_context(|| {
            format!(
                "no connection after {}s: bluetoothd stuck?",
                SETUP.as_secs()
            )
        })??;
    // Subscribe before the first read, and only report "connected" once both worked: a value
    // pushed in between isn't lost, and a hanging StartNotify (phone app never answering the
    // CCCD write) can't leave us half-linked with the phone thinking we're subscribed.
    let notify = call("subscribe STATE", state.notify()).await?;
    info!("phone link: subscribed to STATE");
    let initial = call("read STATE", state.read()).await?;
    let parsed = PhoneState::parse(&initial);
    info!(
        "phone link: first STATE read ({} bytes): {parsed:?}",
        initial.len()
    );
    link.send_replace(Link {
        connected: true,
        state: parsed,
        service_missing: false,
    });
    info!("phone connected: {parsed:?}");

    pin_mut!(notify);
    let mut le_check = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = le_check.tick() => {
                // Only the LE bearer counts: Device1.Connected covers BR/EDR too (KDE Connect)
                // and flips to false on BR/EDR changes while LE stays up.
                // A failing/hanging read means bluetoothd lost the bearer or got stuck: reconnect.
                if !call("LE state", le.connected()).await? {
                    bail!("LE link lost");
                }
            }
            v = notify.next() => {
                let v = v.ok_or_else(|| anyhow!("notifications ended"))?;
                link.send_modify(|l| l.state = PhoneState::parse(&v).or(l.state));
            }
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

/// One bluetoothd call, bounded by CALL.
async fn call<T, E: Into<anyhow::Error>>(
    what: &str,
    f: impl Future<Output = std::result::Result<T, E>>,
) -> Result<T> {
    match timeout(CALL, f).await {
        Ok(r) => r.map_err(Into::into).context(what.to_owned()),
        Err(_) => Err(anyhow!("{what}: bluetoothd not answering")),
    }
}

/// LE-connect the phone (or adopt an LE link that's already up) and find our service.
async fn connect(
    session: &Session,
    adapter: &Adapter,
    addr: Address,
) -> Result<(LeBearer, Characteristic, Characteristic)> {
    if !call("paired?", adapter.device(addr)?.is_paired())
        .await
        .unwrap_or(false)
    {
        bail!("{addr} is not paired: bluetoothctl pair {addr} && bluetoothctl trust {addr}");
    }
    let (device, le) = match call("find LE device", find_le_device(adapter, addr)).await? {
        Some(found) => found,
        None => pair_le(session, adapter, addr).await?,
    };
    let adopted = call("LE state", le.connected()).await?;
    if adopted {
        // BlueZ autoconnect, a Connect of an earlier attempt that landed late, another process:
        // use it. Connect on it would just hang until BlueZ's service discovery is done.
        info!(
            "phone link: LE already connected ({}), using it",
            device.address()
        );
    } else {
        info!("phone link: LE connect to {} starting", device.address());
        if let Err(e) = le_connect(adapter, &device, &le).await {
            let diag = diagnostics(adapter, &device, &le).await;
            bail!("{e:#} [{diag}]");
        }
        info!("phone link: LE connected");
    }
    let wait = if adopted {
        ADOPTED_DISCOVERY
    } else {
        DISCOVERY
    };
    let Some((control, state)) = resolve(&device, &le, wait).await? else {
        if adopted {
            warn!(
                "phone link: adopted LE link has no service discovery after {}s: dropping it for \
                 a fresh connect",
                wait.as_secs()
            );
            le_drop(&le).await;
            bail!("service discovery stalled on adopted LE link: dropped it, reconnecting");
        }
        bail!(
            "service discovery stalled (LE connected, services resolved: false after {}s): \
             retrying",
            wait.as_secs()
        );
    };
    info!("phone link: workstation-connect service found");
    Ok((le, control, state))
}

/// Bring the LE link up; returns as soon as Bearer.LE1 says Connected, without waiting for
/// Connect's reply, which BlueZ only sends after its GATT service discovery.
async fn le_connect(adapter: &Adapter, device: &bluer::Device, le: &LeBearer) -> Result<()> {
    // Kernels before 7.4 (397464709a20) connect from a passive scan filtered on the accept
    // list, which drops the phone's private address when the controller can't resolve it
    // itself. Discovery scans unfiltered and the kernel resolves the address with the IRK.
    let discovery = le_discovery(adapter).await?;
    let connect = le.connect();
    pin_mut!(discovery, connect);
    let mut poll = tokio::time::interval(Duration::from_millis(500));
    let mut connect_done = false;
    // Logs when the phone's advertisement shows up: tells a slow connect apart into "not seen"
    // (phone advertising/scan) vs "seen, connect slow" (controller/link setup).
    let started = std::time::Instant::now();
    let mut seen = false;
    timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                r = &mut connect, if !connect_done => {
                    connect_done = true;
                    if let Err(e) = r {
                        // E.g. zbus' own timeout while the link did come up meanwhile.
                        if call("LE state", le.connected()).await? {
                            warn!("phone link: LE Connect failed ({e:#}) but LE is up: using it");
                            return Ok(());
                        }
                        return Err(e);
                    }
                }
                Some(_) = discovery.next() => {}
                _ = poll.tick() => {
                    if !seen && let Ok(Ok(Some(rssi))) = timeout(Duration::from_secs(2), device.rssi()).await {
                        seen = true;
                        info!("phone link: phone seen in LE scan after {:.1}s (RSSI {rssi})", started.elapsed().as_secs_f32());
                    }
                    if call("LE state", le.connected()).await? {
                        return Ok(());
                    }
                }
            }
        }
    })
    .await
    .context("LE connect timed out (app running? Bluetooth on? in range?)")?
}

/// Disconnect the LE bearer and wait (max 5 s) until it's down. Best effort: the next round
/// copes with a link that's still up.
async fn le_drop(le: &LeBearer) {
    if let Err(e) = call("LE disconnect", le.disconnect()).await {
        warn!("phone link: {e:#}");
    }
    // 5.87's Disconnect doesn't abort a pending Connect: check the bearer actually went down.
    let down = timeout(Duration::from_secs(5), async {
        while call("LE state", le.connected()).await.unwrap_or(true) {
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
    if down.is_err() {
        warn!("phone link: LE still connected 5s after Disconnect");
    }
}

/// Wait for GATT service discovery and find our service in it. None: services still not
/// resolved after `wait` while LE is up (discovery stalled).
async fn resolve(
    device: &bluer::Device,
    le: &LeBearer,
    wait: Duration,
) -> Result<Option<(Characteristic, Characteristic)>> {
    let deadline = tokio::time::Instant::now() + wait;
    let mut logged: Option<usize> = None;
    let (resolved, seen) = loop {
        let le_up = call("LE state", le.connected()).await?;
        if !le_up {
            bail!("link dropped during service discovery (LE disconnected): retrying");
        }
        let resolved = call("ServicesResolved", device.is_services_resolved()).await?;
        let services = if resolved {
            // Errs (unresolved) if the link drops right now; the next round sees that.
            call("GATT services", device.services())
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if resolved && logged != Some(services.len()) {
            info!(
                "phone link: services resolved, {} GATT services",
                services.len()
            );
            logged = Some(services.len());
        }
        if let Some(c) = find_chars(&services).await? {
            return Ok(Some(c));
        }
        if tokio::time::Instant::now() >= deadline {
            let mut seen = Vec::new();
            for s in &services {
                if let Ok(u) = call("service UUID", s.uuid()).await {
                    seen.push(u.to_string());
                }
            }
            break (resolved, seen);
        }
        sleep(Duration::from_millis(500)).await;
    };
    if !resolved {
        return Ok(None);
    }
    if seen.is_empty() {
        bail!(
            "service discovery stalled (LE connected, services resolved, no GATT services): \
             retrying"
        );
    }
    Err(ServiceMissing {
        le_connected: true,
        msg: format!(
            "workstation-connect service not found on phone (LE connected, GATT services: [{}]); \
             phone app's GATT server is gone: toggle Bluetooth off/on on the phone \
             (BlueZ cache: `Cache = no` under [GATT], wsconnect setup checks it)",
            seen.join(", ")
        ),
    }
    .into())
}

/// What BlueZ knows about the phone, for a failed LE connect: was it even visible?
async fn diagnostics(adapter: &Adapter, device: &bluer::Device, le: &LeBearer) -> String {
    async fn probe<T: std::fmt::Debug, E: std::fmt::Display>(
        f: impl Future<Output = std::result::Result<T, E>>,
    ) -> String {
        match timeout(Duration::from_secs(5), f).await {
            Ok(Ok(v)) => format!("{v:?}"),
            Ok(Err(e)) => format!("error: {e}"),
            Err(_) => "no answer".into(),
        }
    }
    format!(
        "RSSI: {}, LE connected: {}, device connected: {}, services resolved: {}, discovering: {}",
        probe(device.rssi()).await,
        probe(le.connected()).await,
        probe(device.is_connected()).await,
        probe(device.is_services_resolved()).await,
        probe(adapter.is_discovering()).await,
    )
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

async fn find_chars(
    services: &[bluer::gatt::remote::Service],
) -> Result<Option<(Characteristic, Characteristic)>> {
    for service in services {
        if call("service UUID", service.uuid()).await? != proto::SERVICE {
            continue;
        }
        let (mut control, mut state) = (None, None);
        for c in call("characteristics", service.characteristics()).await? {
            match call("characteristic UUID", c.uuid()).await? {
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

    async fn disconnect(&self) -> zbus::Result<()> {
        self.0.call("Disconnect", &()).await
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
