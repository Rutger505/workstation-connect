//! The daemon: watches NetworkManager, asks the phone for internet, switches BT → hotspot,
//! switches back to known Wi-Fi, turns everything off on sleep or pause.

use crate::config::Config;
use crate::logic::{self, OfflineTimer};
use crate::net::{self, Nm, Snapshot};
use crate::phone::{Link, Phone};
use crate::proto::{self, Cmd};
use anyhow::{Context, Result, bail};
use futures::{StreamExt, pin_mut};
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Internet without the phone.
    Idle,
    /// No internet, counting down (or no phone in reach).
    Offline,
    /// Asked the phone, bringing up BT / hotspot.
    Activating,
    /// On the phone's Bluetooth tethering (slow), hotspot still coming.
    Bluetooth,
    /// On the phone's hotspot.
    Hotspot,
    /// Failed; waiting before trying again.
    Cooldown,
    Paused,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub mode: Mode,
    pub paused: bool,
    pub phone: Link,
    pub offline_secs: Option<u64>,
    pub error: Option<String>,
    /// Only when the installed wsconnect is behind GitHub main.
    pub update: Option<crate::update::Update>,
}

struct Ctx {
    cfg: Config,
    nm: Nm,
    phone: Phone,
    status: watch::Sender<Status>,
    paused: watch::Sender<bool>,
    /// Bumped on pause and sleep: running flows see the change and bail out without side effects.
    epoch: AtomicU64,
}

impl Ctx {
    fn set(&self, f: impl FnOnce(&mut Status)) {
        self.status.send_if_modified(|s| {
            let before = serde_json::to_string(s).ok();
            f(s);
            before != serde_json::to_string(s).ok()
        });
    }

    fn mode(&self, mode: Mode) {
        self.set(|s| s.mode = mode);
    }

    fn error(&self, e: impl std::fmt::Display) {
        let msg = e.to_string();
        warn!("{msg}");
        self.set(|s| s.error = Some(msg));
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn abort_running(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    fn on_phone(&self) -> bool {
        matches!(
            self.status.borrow().mode,
            Mode::Activating | Mode::Bluetooth | Mode::Hotspot
        )
    }
}

/// Raised when a pause/sleep happened mid-flow.
#[derive(Debug)]
struct Aborted;
impl std::fmt::Display for Aborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("aborted by pause/sleep")
    }
}
impl std::error::Error for Aborted {}

fn check(ctx: &Ctx, epoch: u64) -> Result<()> {
    if ctx.epoch() != epoch || *ctx.paused.borrow() {
        bail!(Aborted);
    }
    Ok(())
}

pub async fn run(cfg: Config) -> Result<()> {
    let addr = cfg
        .phone
        .parse()
        .with_context(|| format!("phone address {:?}", cfg.phone))?;
    let phone = Phone::spawn(addr);
    let (status, _) = watch::channel(Status {
        mode: Mode::Idle,
        paused: false,
        phone: Link::default(),
        offline_secs: None,
        error: None,
        update: None,
    });
    let ctx = Arc::new(Ctx {
        cfg,
        nm: Nm::new().await?,
        phone,
        status,
        paused: watch::channel(false).0,
        epoch: AtomicU64::new(0),
    });

    // Mirror the phone link into the status.
    let c = ctx.clone();
    tokio::spawn(async move {
        let mut rx = c.phone.watch();
        loop {
            let link = *rx.borrow_and_update();
            c.set(|s| s.phone = link);
            if rx.changed().await.is_err() {
                break;
            }
        }
    });
    let c = ctx.clone();
    tokio::spawn(async move { crate::update::watch(|u| c.set(|s| s.update = u)).await });
    tokio::spawn(sleep_guard(ctx.clone()));
    tokio::spawn(wifi_guard(ctx.clone()));
    tokio::spawn(serve_socket(ctx.clone()));

    loop {
        idle(&ctx).await;
        let epoch = ctx.epoch();
        let result = match activate(&ctx, epoch).await {
            Ok(()) => on_phone(&ctx, epoch).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => {}
            Err(e) if e.is::<Aborted>() => info!("{e}"),
            Err(e) => {
                ctx.error(format!("{e:#}"));
                let _ = ctx.phone.send(Cmd::Off, Duration::from_secs(5)).await;
                ctx.mode(Mode::Cooldown);
                sleep(Duration::from_secs(60)).await;
            }
        }
    }
}

/// Returns when the phone should take over (or we're already on it, e.g. after a daemon restart).
async fn idle(ctx: &Ctx) {
    let mut timer = OfflineTimer::default();
    let threshold = Duration::from_secs(ctx.cfg.offline_secs);
    loop {
        if *ctx.paused.borrow() {
            ctx.set(|s| {
                s.mode = Mode::Paused;
                s.offline_secs = None;
            });
            let _ = ctx.paused.subscribe().wait_for(|p| !p).await;
            timer.reset();
        }
        sleep(Duration::from_secs(1)).await;
        if *ctx.paused.borrow() {
            continue;
        }
        let snap = match ctx.nm.snapshot().await {
            Ok(s) => s,
            Err(e) => {
                ctx.error(format!("NetworkManager: {e:#}"));
                continue;
            }
        };
        if snap.is_active(&ctx.cfg.hotspot_connection) || snap.is_active(&ctx.cfg.bt_connection) {
            info!("already on the phone: taking over");
            return;
        }
        let now = Instant::now();
        let trigger = timer.update(snap.online, snap.connecting, now, threshold);
        let offline = timer.offline_for(now).map(|d| d.as_secs());
        ctx.set(|s| {
            s.mode = if snap.online {
                Mode::Idle
            } else {
                Mode::Offline
            };
            s.offline_secs = offline;
            if snap.online {
                s.error = None;
            }
        });
        if !trigger {
            continue;
        }
        let link = ctx.phone.link();
        match link.state {
            Some(st) if link.connected && st.shizuku => return,
            Some(_) if link.connected => {
                ctx.error("phone: Shizuku not running (start it via wireless debugging)")
            }
            _ => ctx.set(|s| s.error = Some("phone not in reach over Bluetooth".into())),
        }
    }
}

async fn activate(ctx: &Ctx, epoch: u64) -> Result<()> {
    let snap = ctx.nm.snapshot().await?;
    if snap.is_active(&ctx.cfg.hotspot_connection) || snap.is_active(&ctx.cfg.bt_connection) {
        ctx.mode(if snap.is_active(&ctx.cfg.hotspot_connection) {
            Mode::Hotspot
        } else {
            Mode::Bluetooth
        });
        return Ok(());
    }
    ctx.set(|s| {
        s.mode = Mode::Activating;
        s.offline_secs = None;
        s.error = None;
    });
    info!("offline: asking the phone");
    drop_dead_wifi(&snap).await;
    ctx.phone.send(Cmd::On, Duration::from_secs(10)).await?;
    check(ctx, epoch)?;

    // Bluetooth first: internet within seconds.
    let mut bt = false;
    if ctx
        .phone
        .wait_for(Duration::from_secs(15), |s| s.bt_tether)
        .await
    {
        for _ in 0..3 {
            check(ctx, epoch)?;
            match net::up(&ctx.cfg.bt_connection, 15).await {
                Ok(()) => {
                    bt = true;
                    break;
                }
                Err(e) => {
                    warn!("bt up: {e:#}");
                    sleep(Duration::from_secs(2)).await;
                }
            }
        }
    } else {
        warn!("phone didn't report BT tethering; going straight for the hotspot");
    }
    check(ctx, epoch)?;
    if bt {
        ctx.mode(Mode::Bluetooth);
        ctx.nm.recheck().await;
    }

    // Then the hotspot: the speed.
    if try_hotspot(ctx, epoch, Duration::from_secs(60)).await? {
        switched_to_hotspot(ctx).await;
        return Ok(());
    }
    if bt {
        warn!("hotspot not up yet; staying on Bluetooth and retrying");
        return Ok(());
    }
    bail!("phone gave neither Bluetooth tethering nor hotspot")
}

/// Wi-Fi that's connected without internet (captive portal) would keep the default route.
async fn drop_dead_wifi(snap: &Snapshot) {
    if snap.online {
        return;
    }
    for c in snap.active.iter().filter(|c| c.kind == "802-11-wireless") {
        info!("dropping Wi-Fi without internet: {}", c.id);
        if let Err(e) = net::down(&c.id).await {
            warn!("{e:#}");
        }
    }
}

async fn try_hotspot(ctx: &Ctx, epoch: u64, within: Duration) -> Result<bool> {
    let deadline = Instant::now() + within;
    ctx.phone
        .wait_for(Duration::from_secs(20), |s| s.hotspot)
        .await;
    while Instant::now() < deadline {
        check(ctx, epoch)?;
        match net::up(&ctx.cfg.hotspot_connection, 15).await {
            Ok(()) => return Ok(true),
            Err(e) => {
                warn!("hotspot up: {e:#}");
                net::rescan().await;
                sleep(Duration::from_secs(3)).await;
            }
        }
    }
    Ok(false)
}

/// Hotspot is up; move off Bluetooth exactly once.
async fn switched_to_hotspot(ctx: &Ctx) {
    ctx.mode(Mode::Hotspot);
    ctx.nm.recheck().await;
    if let Ok(snap) = ctx.nm.snapshot().await
        && snap.is_active(&ctx.cfg.bt_connection)
        && let Err(e) = net::down(&ctx.cfg.bt_connection).await
    {
        warn!("bt down: {e:#}");
    }
    if let Err(e) = ctx.phone.send(Cmd::BtOff, Duration::from_secs(10)).await {
        warn!("{e:#}");
    }
    info!("on the phone's hotspot");
}

/// On the phone until a known Wi-Fi works again, the phone connection drops, or pause/sleep.
async fn on_phone(ctx: &Ctx, epoch: u64) -> Result<()> {
    let hotspot = ctx.cfg.hotspot_connection.as_str();
    let bt = ctx.cfg.bt_connection.as_str();
    let mut tried: HashSet<String> = HashSet::new();
    let mut last_scan = Instant::now();
    let mut last_hotspot_try = Instant::now();
    loop {
        sleep(Duration::from_secs(1)).await;
        check(ctx, epoch)?;
        let snap = ctx.nm.snapshot().await?;
        let (on_hotspot, on_bt) = (snap.is_active(hotspot), snap.is_active(bt));
        if !on_hotspot && !on_bt {
            warn!("phone connection gone");
            let _ = ctx.phone.send(Cmd::Off, Duration::from_secs(5)).await;
            return Ok(()); // idle: asks again if still offline
        }
        ctx.mode(if on_hotspot {
            Mode::Hotspot
        } else {
            Mode::Bluetooth
        });

        if on_bt && !on_hotspot && last_hotspot_try.elapsed() >= Duration::from_secs(20) {
            if try_hotspot(ctx, epoch, Duration::from_secs(15)).await? {
                switched_to_hotspot(ctx).await;
            }
            last_hotspot_try = Instant::now();
        }

        if last_scan.elapsed() >= Duration::from_secs(60) {
            last_scan = Instant::now();
            let mut exclude: Vec<&str> = vec![hotspot, bt];
            exclude.extend(tried.iter().map(String::as_str));
            match net::known_wifi_in_range(&exclude, &ctx.cfg.ignore_ssids).await {
                Ok(Some(id)) => {
                    // Each network gets one try per session: no flapping between hotspot and bad Wi-Fi.
                    tried.insert(id.clone());
                    if try_known_wifi(ctx, epoch, &id).await? {
                        let _ = ctx.phone.send(Cmd::Off, Duration::from_secs(10)).await;
                        info!("back on {id}: phone off");
                        return Ok(());
                    }
                }
                Ok(None) => {}
                Err(e) => warn!("wifi scan: {e:#}"),
            }
        }
    }
}

async fn try_known_wifi(ctx: &Ctx, epoch: u64, id: &str) -> Result<bool> {
    info!("known Wi-Fi in range: trying {id}");
    if net::up(id, 20).await.is_ok() {
        ctx.nm.recheck().await;
        for _ in 0..15 {
            check(ctx, epoch)?;
            let snap = ctx.nm.snapshot().await?;
            if snap.online && snap.is_active(id) {
                if snap.is_active(&ctx.cfg.bt_connection) {
                    let _ = net::down(&ctx.cfg.bt_connection).await;
                }
                return Ok(true);
            }
            sleep(Duration::from_secs(1)).await;
        }
    }
    warn!("{id} has no internet: back to the hotspot");
    if let Err(e) = net::up(&ctx.cfg.hotspot_connection, 20).await {
        warn!("hotspot back up: {e:#}");
    }
    Ok(false)
}

/// The phone's KDE Connect keeps a dead TCP link "connected" for ~15 min after the laptop leaves
/// its network; tell it over BLE whenever the Wi-Fi connection goes away.
async fn wifi_guard(ctx: Arc<Ctx>) {
    loop {
        let res: Result<()> = async {
            let changes = ctx.nm.wifi_connections().await?;
            pin_mut!(changes);
            let Some(mut prev) = changes.next().await else {
                bail!("no Wi-Fi state");
            };
            while let Some(new) = changes.next().await {
                if logic::wifi_left(&prev.0, &new.0) {
                    let (old, new) = (name(&prev.1), name(&new.1));
                    tokio::spawn(tell_net_changed(ctx.clone(), format!("{old} → {new}")));
                }
                prev = new;
            }
            bail!("Wi-Fi watch ended")
        }
        .await;
        if let Err(e) = res {
            warn!("wifi watch: {e:#}");
        }
        sleep(Duration::from_secs(30)).await;
    }
}

fn name(id: &str) -> &str {
    if id.is_empty() { "none" } else { id }
}

/// Fire and forget: a dropped Wi-Fi isn't worth queueing for a phone that's out of reach.
async fn tell_net_changed(ctx: Arc<Ctx>, change: String) {
    let link = ctx.phone.link();
    match link.state.filter(|_| link.connected) {
        None => info!("wifi changed ({change}): phone not connected, not telling it"),
        Some(st) if st.version < proto::NET_CHANGED_VERSION => {
            info!(
                "wifi changed ({change}): phone app v{} too old to drop LAN links",
                st.version
            )
        }
        Some(_) => match ctx
            .phone
            .send(Cmd::NetChanged, Duration::from_secs(3))
            .await
        {
            Ok(()) => info!("wifi changed ({change}): told the phone to drop LAN links"),
            Err(e) => warn!("wifi changed ({change}): {e:#}"),
        },
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Login {
    fn inhibit(
        &self,
        what: &str,
        who: &str,
        why: &str,
        mode: &str,
    ) -> zbus::Result<zbus::zvariant::OwnedFd>;
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

/// Holds a logind delay lock so "phone off" goes out before suspend.
async fn sleep_guard(ctx: Arc<Ctx>) {
    let res: Result<()> = async {
        let bus = zbus::Connection::system().await?;
        let login = LoginProxy::new(&bus).await?;
        let mut signals = login.receive_prepare_for_sleep().await?;
        let mut lock = Some(
            login
                .inhibit("sleep", "wsconnect", "turn the phone hotspot off", "delay")
                .await?,
        );
        while let Some(sig) = signals.next().await {
            if sig.args()?.start {
                if ctx.on_phone() {
                    ctx.abort_running();
                    match ctx.phone.send(Cmd::Off, Duration::from_secs(3)).await {
                        Ok(()) => info!("suspend: phone off"),
                        Err(e) => warn!("suspend: {e:#}"),
                    }
                }
                lock = None; // release: suspend continues
            } else if lock.is_none() {
                lock = Some(
                    login
                        .inhibit("sleep", "wsconnect", "turn the phone hotspot off", "delay")
                        .await?,
                );
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = res {
        ctx.error(format!("sleep guard stopped: {e:#}"));
    }
}

pub fn socket_path() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("wsconnect.sock")
}

async fn serve_socket(ctx: Arc<Ctx>) {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => return ctx.error(format!("socket {}: {e}", path.display())),
    };
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = client(ctx, stream).await {
                tracing::debug!("client: {e:#}");
            }
        });
    }
}

async fn client(ctx: Arc<Ctx>, stream: UnixStream) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await?;
    let pause = |on: bool| {
        if on == *ctx.paused.borrow() {
            return;
        }
        ctx.paused.send_replace(on);
        ctx.set(|s| s.paused = on);
        if on {
            let was_on_phone = ctx.on_phone();
            ctx.abort_running();
            ctx.mode(Mode::Paused);
            if was_on_phone {
                let c = ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = c.phone.send(Cmd::Off, Duration::from_secs(10)).await {
                        warn!("pause: {e:#}");
                    }
                });
            }
        }
        tokio::spawn(async move {
            if let Err(e) = crate::kde::pause_sync(on).await {
                warn!("kde connect pause={on}: {e:#}");
            }
        });
        info!("paused: {on}");
    };
    match line.trim() {
        "status" => {}
        "pause" => pause(true),
        "resume" => pause(false),
        "toggle" => pause(!*ctx.paused.borrow()),
        "watch" => {
            let mut rx = ctx.status.subscribe();
            loop {
                let s = serde_json::to_string(&*rx.borrow_and_update())?;
                w.write_all(format!("{s}\n").as_bytes()).await?;
                rx.changed().await?;
            }
        }
        other => bail!("unknown command {other:?}"),
    }
    let s = serde_json::to_string(&*ctx.status.borrow())?;
    w.write_all(format!("{s}\n").as_bytes()).await?;
    Ok(())
}
