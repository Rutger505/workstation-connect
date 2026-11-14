//! BLE link to the phone: keeps a GATT connection open, streams its state, sends commands.

use crate::proto::{self, Cmd, PhoneState};
use anyhow::{Context, Result, anyhow, bail};
use bluer::gatt::WriteOp;
use bluer::gatt::remote::{Characteristic, CharacteristicWriteRequest};
use bluer::{Adapter, Address, DeviceEvent, DeviceProperty, DiscoveryFilter, DiscoveryTransport};
use futures::{StreamExt, pin_mut};
use serde::Serialize;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{sleep, timeout};
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Link {
    pub connected: bool,
    pub state: Option<PhoneState>,
}

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
    loop {
        let started = std::time::Instant::now();
        let res = async {
            let session = bluer::Session::new().await?;
            let adapter = session.default_adapter().await?;
            if !adapter.is_powered().await? {
                bail!("bluetooth adapter is off");
            }
            connect_and_serve(&adapter, addr, &mut cmds, &link).await
        }
        .await;
        link.send_replace(Link::default());
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
    adapter: &Adapter,
    addr: Address,
    cmds: &mut mpsc::Receiver<Request>,
    link: &watch::Sender<Link>,
) -> Result<()> {
    let device = adapter.device(addr)?;
    if !device.is_paired().await.unwrap_or(false) {
        bail!("{addr} is not paired: bluetoothctl pair {addr} && bluetoothctl trust {addr}");
    }
    if !device.is_connected().await? || !device.is_services_resolved().await? {
        wait_for_advert(adapter, addr).await?;
        prefer_le(adapter.name(), addr).await;
        timeout(Duration::from_secs(20), device.connect())
            .await
            .context("connect timed out")??;
    }
    timeout(Duration::from_secs(15), async {
        while !device.is_services_resolved().await? {
            sleep(Duration::from_millis(250)).await;
        }
        Ok::<_, bluer::Error>(())
    })
    .await
    .context("GATT services not resolved")??;

    let (control, state) = find_chars(&device).await?;
    let initial = state.read().await.context("read state (bonded over LE?)")?;
    link.send_replace(Link {
        connected: true,
        state: PhoneState::parse(&initial),
    });
    info!("phone connected: {:?}", link.borrow().state);

    let notify = state.notify().await.context("subscribe state")?;
    let events = device.events().await?;
    pin_mut!(notify, events);
    loop {
        tokio::select! {
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
                let res = control.write_ext(&[cmd as u8], &req).await;
                info!("sent {cmd:?}: {res:?}");
                let failed = res.is_err();
                let _ = reply.send(res.map_err(|e| anyhow!("write {cmd:?}: {e}")));
                if failed { bail!("write failed"); }
            }
        }
    }
}

/// Run LE discovery until the phone's adverts are seen, so BlueZ connects over LE, not BR/EDR.
async fn wait_for_advert(adapter: &Adapter, addr: Address) -> Result<()> {
    adapter
        .set_discovery_filter(DiscoveryFilter {
            uuids: HashSet::from([proto::SERVICE]),
            transport: DiscoveryTransport::Le,
            ..Default::default()
        })
        .await?;
    let discovery = adapter.discover_devices().await?;
    pin_mut!(discovery);
    let device = adapter.device(addr)?;
    let seen = timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                _ = discovery.next() => {}
                _ = sleep(Duration::from_millis(500)) => {}
            }
            if device.rssi().await.ok().flatten().is_some() {
                return;
            }
        }
    })
    .await;
    if seen.is_err() {
        bail!("phone not advertising (app running? Bluetooth on? in range?)");
    }
    Ok(())
}

/// BlueZ >= 5.77: make Device1.Connect pick LE for this dual-mode phone. Best effort.
async fn prefer_le(adapter: &str, addr: Address) {
    let path = format!(
        "/org/bluez/{adapter}/dev_{}",
        addr.to_string().replace(':', "_")
    );
    let res: zbus::Result<()> = async {
        let bus = zbus::Connection::system().await?;
        let props = zbus::fdo::PropertiesProxy::builder(&bus)
            .destination("org.bluez")?
            .path(path.as_str())?
            .build()
            .await?;
        let iface = zbus::names::InterfaceName::try_from("org.bluez.Device1")?;
        props.set(iface, "PreferredBearer", "le".into()).await?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::debug!("PreferredBearer=le not set: {e}");
    }
}

async fn find_chars(device: &bluer::Device) -> Result<(Characteristic, Characteristic)> {
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
        return Ok((
            control.context("no control characteristic")?,
            state.context("no state characteristic")?,
        ));
    }
    bail!("workstation-connect service not found on phone (app outdated or service stopped?)")
}
