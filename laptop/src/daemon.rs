//! The daemon: watches NetworkManager, asks the phone for its hotspot when offline, joins it,
//! switches back to known Wi-Fi, turns everything off on sleep or pause.

use crate::config::Config;
use crate::kde::DeviceInfo;
use crate::logic::{self, OfflineTimer};
use crate::logs::{Reason, Uploader};
use crate::net::{self, Nm, Snapshot};
use crate::phone::{Link, LinkEvent, Phone};
use crate::proto::{self, Cmd};
use crate::selftest::{Res, Step};
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
    /// Asked the phone, joining its hotspot.
    Activating,
    /// On the phone's hotspot.
    Hotspot,
    /// Failed; waiting before trying again.
    Cooldown,
    Paused,
    /// `wsconnect selftest` holds the network (hotspot test); auto logic waits.
    Selftest,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// Running daemon's version (may differ from the installed CLI until restarted).
    pub version: &'static str,
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
    /// Selftest's hotspot test runs: auto logic waits, running flows abort.
    hold: watch::Sender<bool>,
    /// Bumped on pause and sleep: running flows see the change and bail out without side effects.
    epoch: AtomicU64,
    /// Bumped on every resume from suspend: offline timer and cooldown start over.
    resumes: watch::Sender<u64>,
    uploader: Uploader,
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

    /// Already on the phone's hotspot.
    fn on_hotspot(&self, snap: &Snapshot) -> bool {
        snap.is_active(&self.cfg.hotspot_connection)
    }

    fn on_phone(&self) -> bool {
        matches!(self.status.borrow().mode, Mode::Activating | Mode::Hotspot)
    }
}

/// Raised when a pause/sleep/selftest happened mid-flow.
#[derive(Debug)]
struct Aborted;
impl std::fmt::Display for Aborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("aborted by pause/sleep/selftest")
    }
}
impl std::error::Error for Aborted {}

fn check(ctx: &Ctx, epoch: u64) -> Result<()> {
    if ctx.epoch() != epoch || *ctx.paused.borrow() || *ctx.hold.borrow() {
        bail!(Aborted);
    }
    Ok(())
}

pub async fn run(cfg: Config) -> Result<()> {
    for key in cfg.obsolete_keys() {
        warn!("config: `{key}` is obsolete (Bluetooth tethering is gone), ignored: remove it");
    }
    if let Err(e) = net::ensure_hidden(&cfg.hotspot_connection).await {
        warn!("hotspot connection: {e:#}");
    }
    let addr = cfg
        .phone
        .parse()
        .with_context(|| format!("phone address {:?}", cfg.phone))?;
    let phone = Phone::spawn(addr);
    let (status, _) = watch::channel(Status {
        version: crate::update::VERSION,
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
        hold: watch::channel(false).0,
        epoch: AtomicU64::new(0),
        resumes: watch::channel(0).0,
        uploader: Uploader::spawn(),
    });
    info!("wsconnect {} started", crate::update::VERSION);
    ctx.uploader.trigger(Reason::Start);

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
    let reconnected = Arc::new(tokio::sync::Notify::new());
    let r = reconnected.clone();
    let auto = ctx.cfg.auto_update;
    let up = ctx.uploader.clone();
    tokio::spawn(async move {
        crate::update::watch(
            auto,
            |u| c.set(|s| s.update = u),
            || up.trigger(Reason::Update),
            &r,
        )
        .await
    });
    tokio::spawn(link_events(ctx.clone()));
    tokio::spawn(periodic_upload(ctx.clone()));
    tokio::spawn(kde_watch(ctx.clone()));
    tokio::spawn(reconnect_watch(ctx.clone(), reconnected));
    tokio::spawn(sleep_guard(ctx.clone()));
    tokio::spawn(wifi_guard(ctx.clone()));
    tokio::spawn(serve_socket(ctx.clone()));

    loop {
        idle(&ctx).await;
        let epoch = ctx.epoch();
        let result = match activate(&ctx, epoch).await {
            Ok(()) => {
                ctx.uploader.trigger(Reason::HotspotOk);
                on_phone(&ctx, epoch).await
            }
            Err(e) => {
                if !e.is::<Aborted>() {
                    ctx.uploader.trigger(Reason::HotspotFail);
                }
                Err(e)
            }
        };
        match result {
            Ok(()) => {}
            Err(e) if e.is::<Aborted>() => info!("{e}"),
            Err(e) => {
                ctx.error(format!("{e:#}"));
                let _ = ctx.phone.send(Cmd::Off, Duration::from_secs(5)).await;
                ctx.mode(Mode::Cooldown);
                let mut resumes = ctx.resumes.subscribe();
                tokio::select! {
                    _ = sleep(Duration::from_secs(60)) => {}
                    _ = resumes.changed() => info!("resumed: cooldown over"),
                }
            }
        }
    }
}

/// Returns when the phone should take over (or we're already on it, e.g. after a daemon restart).
async fn idle(ctx: &Ctx) {
    let mut timer = OfflineTimer::default();
    let threshold = Duration::from_secs(ctx.cfg.offline_secs);
    let mut resumes = ctx.resumes.subscribe();
    let exclude = [ctx.cfg.hotspot_connection.as_str()];
    loop {
        if *ctx.hold.borrow() {
            ctx.set(|s| {
                s.mode = Mode::Selftest;
                s.offline_secs = None;
            });
            let _ = ctx.hold.subscribe().wait_for(|h| !h).await;
            timer.reset();
            continue;
        }
        if *ctx.paused.borrow() {
            ctx.set(|s| {
                s.mode = Mode::Paused;
                s.offline_secs = None;
            });
            let _ = ctx.paused.subscribe().wait_for(|p| !p).await;
            timer.reset();
        }
        sleep(Duration::from_secs(1)).await;
        if *ctx.paused.borrow() || *ctx.hold.borrow() {
            continue;
        }
        if resumes.has_changed().unwrap_or(false) {
            resumes.mark_unchanged();
            timer.reset(); // pre-suspend scan says nothing about where we woke up
        }
        let snap = match ctx.nm.snapshot().await {
            Ok(s) => s,
            Err(e) => {
                ctx.error(format!("NetworkManager: {e:#}"));
                continue;
            }
        };
        if ctx.on_hotspot(&snap) {
            info!("already on the phone: taking over");
            return;
        }
        let mut now = Instant::now();
        timer.update(snap.online, snap.connecting, now, threshold);
        if timer.needs_scan(now) {
            ctx.set(|s| s.mode = Mode::Offline);
            let known = match net::known_wifi_visible(&exclude, &ctx.cfg.ignore_ssids).await {
                Ok(k) => {
                    info!(
                        "offline: {}",
                        if k {
                            "known Wi-Fi in range, giving NetworkManager its chance"
                        } else {
                            "no known Wi-Fi in range, asking the phone now"
                        }
                    );
                    Some(k)
                }
                Err(e) => {
                    warn!("offline scan: {e:#}");
                    None
                }
            };
            now = Instant::now();
            timer.scanned(known, now);
        }
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
    if ctx.on_hotspot(&snap) {
        ctx.mode(Mode::Hotspot);
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
    if !try_hotspot(ctx, epoch, Duration::from_secs(45)).await? {
        if ctx.phone.link().state.is_some_and(|s| s.error) {
            warn!("phone reports a failed hotspot command (see its app log)");
        }
        bail!("phone gave no hotspot");
    }
    ctx.mode(Mode::Hotspot);
    ctx.nm.recheck().await;
    info!("on the phone's hotspot");
    Ok(())
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

/// Waits (≤20 s) for the phone's hotspot flag, then joins, rescanning between tries.
async fn try_hotspot(ctx: &Ctx, epoch: u64, within: Duration) -> Result<bool> {
    let hotspot = ctx.cfg.hotspot_connection.as_str();
    let start = Instant::now();
    let deadline = start + within;
    // The phone's hotspot is up ~3 s after "on". The connection probes its SSID directly
    // (hidden = yes), so no waiting for a scan that lists it.
    ctx.phone
        .wait_for(Duration::from_secs(10), |s| s.hotspot)
        .await;
    info!(
        "phone hotspot flag after {:.1}s",
        start.elapsed().as_secs_f32()
    );
    for attempt in 1.. {
        check(ctx, epoch)?;
        match net::up(hotspot, 10).await {
            Ok(()) => {
                info!(
                    "hotspot up (attempt {attempt}, {:.1}s after on)",
                    start.elapsed().as_secs_f32()
                );
                return Ok(true);
            }
            Err(e) => {
                warn!("hotspot up (attempt {attempt}): {e:#}");
                if Instant::now() >= deadline {
                    break;
                }
                net::rescan().await;
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
    Ok(false)
}

/// On the phone until a known Wi-Fi works again, the hotspot drops, or pause/sleep.
async fn on_phone(ctx: &Ctx, epoch: u64) -> Result<()> {
    let hotspot = ctx.cfg.hotspot_connection.as_str();
    let mut tried: HashSet<String> = HashSet::new();
    let mut last_scan = Instant::now();
    loop {
        sleep(Duration::from_secs(1)).await;
        check(ctx, epoch)?;
        let snap = ctx.nm.snapshot().await?;
        if !ctx.on_hotspot(&snap) {
            warn!("phone connection gone");
            let _ = ctx.phone.send(Cmd::Off, Duration::from_secs(5)).await;
            return Ok(()); // idle: asks again if still offline
        }
        ctx.mode(Mode::Hotspot);

        if last_scan.elapsed() >= Duration::from_secs(60) {
            last_scan = Instant::now();
            let mut exclude: Vec<&str> = vec![hotspot];
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

/// The Wi-Fi replaces the hotspot on the same radio, so a failure brings the hotspot back.
async fn try_known_wifi(ctx: &Ctx, epoch: u64, id: &str) -> Result<bool> {
    info!("known Wi-Fi in range: trying {id}");
    if net::up(id, 20).await.is_ok() {
        ctx.nm.recheck().await;
        for _ in 0..15 {
            check(ctx, epoch)?;
            let snap = ctx.nm.snapshot().await?;
            if snap.online && snap.is_active(id) {
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

/// Log upload after every phone link connect outcome.
async fn link_events(ctx: Arc<Ctx>) {
    let mut rx = ctx.phone.events();
    loop {
        match rx.recv().await {
            Ok(LinkEvent::Connected) => ctx.uploader.trigger(Reason::LinkOk),
            Ok(LinkEvent::GaveUp) => ctx.uploader.trigger(Reason::LinkFail),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => break,
        }
    }
}

/// Log upload every 10 min while online (skipped without new lines).
async fn periodic_upload(ctx: Arc<Ctx>) {
    loop {
        sleep(Duration::from_secs(10 * 60)).await;
        if ctx.nm.snapshot().await.is_ok_and(|s| s.online) {
            ctx.uploader.trigger(Reason::Periodic);
        }
    }
}

async fn kde_info(kde: &mut Option<crate::kde::Kde>, want: Option<&str>) -> Result<DeviceInfo> {
    if kde.is_none() {
        *kde = Some(crate::kde::Kde::connect().await?);
    }
    let k = kde.as_ref().expect("set above");
    let id = k.find_phone(want).await?;
    k.device_info(&id).await
}

/// Logs every change of the phone's KDE Connect device (reachable, link types, loaded plugins)
/// and kdeconnectd D-Bus errors, for debugging input/notifications/clipboard from uploaded logs.
async fn kde_watch(ctx: Arc<Ctx>) {
    let want = ctx.cfg.kdeconnect_device.clone();
    let mut kde = None;
    let mut last: Option<DeviceInfo> = None;
    let mut last_err: Option<String> = None;
    loop {
        match kde_info(&mut kde, want.as_deref()).await {
            Ok(info) => {
                for line in crate::kde::describe_change(last.as_ref(), &info) {
                    info!("{line}");
                }
                last = Some(info);
                last_err = None;
            }
            Err(e) => {
                kde = None; // reconnect: bus or kdeconnectd restarted
                let e = format!("{e:#}");
                if last_err.as_ref() != Some(&e) {
                    warn!("kde connect: {e}");
                    last_err = Some(e);
                }
                last = None;
            }
        }
        sleep(Duration::from_secs(15)).await;
    }
}

/// Pokes the update check on every network (re)connect.
async fn reconnect_watch(ctx: Arc<Ctx>, reconnected: Arc<tokio::sync::Notify>) {
    loop {
        let res: Result<()> = async {
            let events = ctx.nm.reconnects().await?;
            pin_mut!(events);
            // Each property stream starts with its current value: the startup check covers those.
            let start = std::time::Instant::now();
            while events.next().await.is_some() {
                if start.elapsed() > Duration::from_secs(5) {
                    reconnected.notify_one();
                    ctx.uploader.retry();
                }
            }
            bail!("NM watch ended")
        }
        .await;
        if let Err(e) = res {
            warn!("reconnect watch: {e:#}");
        }
        sleep(Duration::from_secs(30)).await;
    }
}

/// The phone's KDE Connect keeps a dead TCP link "connected" for ~15 min after the laptop leaves
/// its network; tell it over Bluetooth whenever the Wi-Fi connection goes away.
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
            } else {
                info!("resumed");
                ctx.resumes.send_modify(|n| *n += 1);
                ctx.phone.kick();
            }
            if !sig.args()?.start && lock.is_none() {
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
    if line.trim_start().starts_with('{') {
        let reply = match serde_json::from_str::<CtlReq>(line.trim()) {
            Ok(req) => handle(&ctx, req).await,
            Err(e) => serde_json::json!({"error": format!("bad request: {e}")}),
        };
        w.write_all(format!("{reply}\n").as_bytes()).await?;
        return Ok(());
    }
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
        "toggle" => {
            // copy first: a borrow() guard alive during send_replace deadlocks the channel
            let on = *ctx.paused.borrow();
            pause(!on)
        }
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

/// JSON requests on the ctl socket (one line, `{"cmd": …}`); answer: one JSON line, `{"error": …}`
/// on failure.
#[derive(serde::Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum CtlReq {
    SelftestLink,
    SelftestHotspot,
    Upload {
        reason: String,
        #[serde(default)]
        report: Option<String>,
    },
}

async fn handle(ctx: &Arc<Ctx>, req: CtlReq) -> serde_json::Value {
    use serde_json::json;
    match req {
        CtlReq::SelftestLink => json!({"steps": selftest_link(ctx).await}),
        CtlReq::SelftestHotspot => json!({"steps": selftest_hotspot(ctx).await}),
        CtlReq::Upload { reason, report } => match Reason::parse(&reason) {
            None => json!({"error": format!("unknown reason {reason:?}")}),
            Some(r) => match ctx.uploader.now(r, report).await {
                Ok(key) => json!({"key": key}),
                Err(e) => json!({"error": format!("{e:#}")}),
            },
        },
    }
}

/// Selftest b: connect now if needed (timed), ping round trip, phone state.
async fn selftest_link(ctx: &Ctx) -> Vec<Step> {
    let mut steps = Vec::new();
    if ctx.phone.link().connected {
        steps.push(Step::pass("connected", None, "already connected"));
    } else {
        info!("selftest: phone link not connected, connecting now");
        let t = Instant::now();
        ctx.phone.kick();
        let mut rx = ctx.phone.watch();
        let ok = tokio::time::timeout(Duration::from_secs(30), rx.wait_for(|l| l.connected))
            .await
            .is_ok_and(|r| r.is_ok());
        if ok {
            steps.push(Step::pass("connect now", Some(t), "connected"));
        } else {
            let link = ctx.phone.link();
            let why = match (link.problem, ctx.phone.last_error()) {
                (Some(p), e) => format!("{p:?}; last error: {}", e.unwrap_or_default()),
                (None, Some(e)) => e,
                (None, None) => "no error recorded".into(),
            };
            steps.push(Step::fail(
                "connect now",
                Some(t),
                format!("not connected after 30 s: {why}"),
            ));
            return steps;
        }
    }
    steps.push(match ctx.phone.ping(Duration::from_secs(5)).await {
        Ok(rtt) => Step::new(
            "ping round trip",
            Res::Pass,
            Some(rtt.as_millis() as u64),
            "state frame back",
        ),
        Err(e) => Step::fail("ping round trip", None, format!("{e:#}")),
    });
    match ctx.phone.link().state {
        Some(st) => {
            let detail = format!(
                "battery {}%, signal {}, app protocol v{}, Shizuku ready {}, hotspot on {}, \
                 wanted {}, last command failed {}",
                st.battery,
                st.signal.map_or("unknown".into(), |s| format!("{s}/4")),
                st.version,
                st.shizuku,
                st.hotspot,
                st.wanted,
                st.error
            );
            steps.push(if st.shizuku {
                Step::pass("phone state", None, detail)
            } else {
                Step::fail(
                    "phone state",
                    None,
                    format!("{detail}: Shizuku not running, the hotspot can't start"),
                )
            });
        }
        None => steps.push(Step::fail("phone state", None, "no state from the phone")),
    }
    steps
}

/// Selftest c, inside the daemon: holds the auto logic, turns the hotspot on, joins it, checks
/// internet, turns it off and rejoins the previous Wi-Fi. Restores also on error/timeout.
async fn selftest_hotspot(ctx: &Arc<Ctx>) -> Vec<Step> {
    if ctx.hold.send_replace(true) {
        return vec![Step::fail(
            "hotspot test",
            None,
            "another selftest is running",
        )];
    }
    info!("selftest: hotspot test, auto logic on hold");
    ctx.abort_running();
    ctx.mode(Mode::Selftest);
    // Running flows notice the hold at their next check (≤1 s).
    sleep(Duration::from_millis(1500)).await;
    let hotspot = ctx.cfg.hotspot_connection.clone();
    let mut steps = Vec::new();
    let (was_on_hotspot, previous) = match ctx.nm.snapshot().await {
        Ok(snap) => (
            ctx.on_hotspot(&snap),
            snap.active
                .iter()
                .find(|c| c.kind == "802-11-wireless" && c.id != hotspot)
                .map(|c| c.id.clone()),
        ),
        Err(e) => {
            steps.push(Step::fail("NetworkManager", None, format!("{e:#}")));
            (false, None)
        }
    };
    steps.push(Step::pass(
        "before",
        None,
        match (&previous, was_on_hotspot) {
            (_, true) => "on the hotspot already".to_string(),
            (Some(p), _) => format!("Wi-Fi {p:?}"),
            (None, _) => "no Wi-Fi".to_string(),
        },
    ));
    let t = Instant::now();
    if tokio::time::timeout(
        Duration::from_secs(60),
        hotspot_steps(ctx, &hotspot, was_on_hotspot, &mut steps),
    )
    .await
    .is_err()
    {
        steps.push(Step::fail("hotspot test", Some(t), "timed out after 60 s"));
    }
    if was_on_hotspot {
        steps.push(Step::skip("restore", "was on the hotspot before: left on"));
    } else {
        let t = Instant::now();
        steps.push(Step::timed(
            "phone off",
            t,
            ctx.phone
                .send(Cmd::Off, Duration::from_secs(10))
                .await
                .map(|()| "sent".into()),
        ));
        let _ = net::down(&hotspot).await; // usually gone already with the phone's hotspot
        match &previous {
            Some(p) => {
                let t = Instant::now();
                steps.push(Step::timed(
                    "restore",
                    t,
                    net::up(p, 20).await.map(|()| format!("back on {p:?}")),
                ));
            }
            None => steps.push(Step::skip("restore", "no Wi-Fi before")),
        }
    }
    ctx.hold.send_replace(false);
    info!("selftest: hotspot test done, auto logic back");
    steps
}

async fn hotspot_steps(ctx: &Ctx, hotspot: &str, already: bool, steps: &mut Vec<Step>) {
    let on = Instant::now();
    let r = ctx.phone.send(Cmd::On, Duration::from_secs(10)).await;
    let failed = r.is_err();
    steps.push(Step::timed("phone on", on, r.map(|()| "sent".into())));
    if failed {
        return;
    }
    let up = ctx
        .phone
        .wait_for(Duration::from_secs(20), |s| s.hotspot)
        .await;
    steps.push(if up {
        Step::pass("phone reports hotspot up", Some(on), "since on")
    } else {
        Step::fail(
            "phone reports hotspot up",
            Some(on),
            "no hotspot flag within 20 s",
        )
    });
    info!(
        "selftest: hotspot flag {up} after {} ms",
        on.elapsed().as_millis()
    );
    if already {
        steps.push(Step::skip("join hotspot", "already connected"));
    } else {
        let t = Instant::now();
        let mut res = Err(anyhow::anyhow!("not tried"));
        for attempt in 1..=3 {
            res = net::up(hotspot, 10).await;
            match &res {
                Ok(()) => break,
                Err(e) => warn!("selftest: hotspot up (attempt {attempt}): {e:#}"),
            }
            net::rescan().await;
            sleep(Duration::from_secs(1)).await;
        }
        let failed = res.is_err();
        steps.push(Step::timed(
            "join hotspot",
            t,
            res.map(|()| format!("{hotspot:?} up")),
        ));
        if failed {
            return;
        }
    }
    let t = Instant::now();
    let mut last = anyhow::anyhow!("not tried");
    loop {
        match tokio::task::spawn_blocking(internet_check).await {
            Ok(Ok(())) => {
                steps.push(Step::pass("internet", Some(t), format!("GET {CHECK_URL}")));
                break;
            }
            Ok(Err(e)) => last = e,
            Err(e) => last = e.into(),
        }
        if t.elapsed() > Duration::from_secs(15) {
            steps.push(Step::fail("internet", Some(t), format!("{last:#}")));
            break;
        }
        sleep(Duration::from_secs(1)).await;
    }
    info!(
        "selftest: internet check done after {} ms",
        t.elapsed().as_millis()
    );
}

/// Arch's NetworkManager connectivity check.
const CHECK_URL: &str = "http://ping.archlinux.org/nm-check.txt";

fn internet_check() -> Result<()> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into();
    let body = agent
        .get(CHECK_URL)
        .call()
        .with_context(|| format!("GET {CHECK_URL}"))?
        .body_mut()
        .read_to_string()?;
    if !body.contains("NetworkManager is online") {
        bail!("GET {CHECK_URL}: unexpected answer (captive portal?)");
    }
    Ok(())
}
