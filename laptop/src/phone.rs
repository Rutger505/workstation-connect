//! Classic Bluetooth link to the phone: keeps an RFCOMM socket open while awake (like earbuds),
//! streams its state, sends commands.
//!
//! Connect: register a client profile for our service UUID and ConnectProfile the bonded phone;
//! BlueZ pages it, finds the RFCOMM channel via SDP and hands us the socket (NewConnection).

use crate::proto::{self, Cmd, Decoder, PhoneState};
use anyhow::{Context, Result, anyhow, bail};
use bluer::rfcomm::stream::{OwnedReadHalf, OwnedWriteHalf};
use bluer::rfcomm::{Profile, ProfileHandle, ReqError, Role, Stream};
use bluer::{Address, ErrorKind, Session};
use futures::{StreamExt, pin_mut};
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::time::{sleep, sleep_until, timeout};
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Link {
    pub connected: bool,
    pub state: Option<PhoneState>,
    /// Why the link can't come up, when it's something the user has to fix.
    pub problem: Option<Problem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Problem {
    /// No classic Bluetooth bond with the phone (or it was lost).
    NotPaired,
    /// Phone reachable, but its app's RFCOMM service didn't answer several tries in a row.
    ServiceMissing,
}

/// Any single D-Bus call to bluetoothd. bluer's own is 120 s: a bluetoothd that never answers
/// (seen after resume from hibernate) hung the daemon silently.
const CALL: Duration = Duration::from_secs(30);
/// ConnectProfile: page + SDP + RFCOMM connect + handing us the socket.
const CONNECT: Duration = Duration::from_secs(30);
/// The phone sends its state right after accepting.
const FIRST_STATE: Duration = Duration::from_secs(10);
const WRITE: Duration = Duration::from_secs(10);
/// Idle link: ping this often so a dead socket is noticed even without traffic.
const PING_EVERY: Duration = Duration::from_secs(60);
const PING_ANSWER: Duration = Duration::from_secs(15);
/// After resume: the socket may have died in the suspend; check fast.
const RESUME_ANSWER: Duration = Duration::from_secs(5);
/// Retry delays (s) when not connected, outside the fast window after resume.
const BACKOFF: [u64; 5] = [2, 5, 10, 30, 60];
/// After resume the adapter takes a few seconds to come back ("target object ... not present"):
/// retry every second meanwhile instead of backing off.
const FAST_WINDOW: Duration = Duration::from_secs(20);
/// Consecutive "service didn't answer" tries before the tray warns.
const MISSING_WARN: u32 = 3;

/// A failure the user has to act on.
#[derive(Debug)]
struct Diagnosed {
    problem: Problem,
    msg: String,
}

impl std::fmt::Display for Diagnosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Diagnosed {}

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

    /// After resume: reconnect now instead of finishing the retry backoff (tokio timers stand
    /// still while suspended, so a 30 s backoff is still 30 s), or check a link that's still up.
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
    let mut step = 0;
    let mut fast_until = Instant::now();
    let mut missing = 0;
    loop {
        let started = Instant::now();
        let res = attempt(addr, &mut cmds, &link, &kick, &mut fast_until).await;
        let was_connected = link.borrow().connected;
        let diagnosed = res
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<Diagnosed>())
            .map(|d| d.problem);
        missing = match diagnosed {
            Some(Problem::ServiceMissing) => missing + 1,
            _ => 0,
        };
        let problem = match diagnosed {
            Some(Problem::ServiceMissing) if missing < MISSING_WARN => None,
            p => p,
        };
        link.send_replace(Link {
            problem,
            ..Default::default()
        });
        if let Err(e) = res {
            warn!("phone link: {e:#}");
        }
        // Commands queued while offline would be stale; fail them now.
        while let Ok((cmd, reply)) = cmds.try_recv() {
            let _ = reply.send(Err(anyhow!("{cmd:?}: phone disconnected")));
        }
        if was_connected && started.elapsed() > Duration::from_secs(60) {
            step = 0; // was a real session, not a failing retry
        }
        let fast = Instant::now() < fast_until;
        let delay = if fast {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(BACKOFF[step])
        };
        tokio::select! {
            _ = sleep(delay) => if !fast {
                step = (step + 1).min(BACKOFF.len() - 1);
            },
            _ = kick.notified() => {
                info!("phone link: reconnecting now (resume)");
                step = 0;
                fast_until = Instant::now() + FAST_WINDOW;
            }
        }
    }
}

/// One connect + serve round; returns when the link is gone.
async fn attempt(
    addr: Address,
    cmds: &mut mpsc::Receiver<Request>,
    link: &watch::Sender<Link>,
    kick: &Notify,
    fast_until: &mut Instant,
) -> Result<()> {
    let started = Instant::now();
    let (session, adapter) = call("open session", async {
        let session = Session::new().await?;
        let adapter = session.default_adapter().await?;
        bluer::Result::Ok((session, adapter))
    })
    .await?;
    if !call("adapter powered", adapter.is_powered()).await? {
        bail!("bluetooth adapter is off");
    }
    let device = adapter.device(addr)?;
    let paired = match timeout(CALL, device.is_paired()).await {
        Err(_) => bail!("paired?: bluetoothd not answering"),
        Ok(Err(e)) if e.kind == ErrorKind::NotFound => false,
        Ok(r) => r.context("paired?")?,
    };
    if !paired {
        return Err(Diagnosed {
            problem: Problem::NotPaired,
            msg: format!(
                "phone {addr} is not paired: pair it in the Bluetooth menu (phone: Settings → \
                 Connected devices → Pair new device)"
            ),
        }
        .into());
    }
    // Dropping it unregisters the profile, which also closes the socket: keep it for the session.
    let mut profile = call(
        "register profile",
        session.register_profile(Profile {
            uuid: proto::SERVICE,
            name: Some("Workstation Connect".into()),
            role: Some(Role::Client),
            require_authentication: Some(true),
            require_authorization: Some(false),
            // We decide when to connect.
            auto_connect: Some(false),
            ..Default::default()
        }),
    )
    .await?;

    info!("phone link: connecting to {addr} (page + SDP + RFCOMM)");
    let connecting = Instant::now();
    let stream = timeout(CONNECT, connect(&device, &mut profile, addr))
        .await
        .map_err(|_| {
            anyhow!(
                "no connection after {}s (phone in range? Bluetooth on?)",
                CONNECT.as_secs()
            )
        })??;
    info!(
        "phone link: RFCOMM connected in {:.1}s",
        connecting.elapsed().as_secs_f32()
    );

    let (mut rd, mut wr) = stream.into_split();
    let mut dec = Decoder::default();
    let state = timeout(FIRST_STATE, async {
        loop {
            let f = next_frame(&mut rd, &mut dec).await?;
            if f.is_empty() {
                continue;
            }
            match PhoneState::parse(&f) {
                Some(s) => return anyhow::Ok(s),
                None => warn!("phone link: bad state frame {f:?}"),
            }
        }
    })
    .await
    .map_err(|_| anyhow!("connected, but no state from the phone within {FIRST_STATE:?}"))??;
    link.send_replace(Link {
        connected: true,
        state: Some(state),
        problem: None,
    });
    info!(
        "phone connected: first state after {:.1}s ({:.1}s since session start): {state:?}",
        connecting.elapsed().as_secs_f32(),
        started.elapsed().as_secs_f32()
    );

    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Some while a ping is out and no frame arrived since.
    let mut answer_by: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            f = next_frame(&mut rd, &mut dec) => {
                let f = f?;
                answer_by = None;
                if f.is_empty() {
                    continue; // keepalive
                }
                match PhoneState::parse(&f) {
                    Some(s) => link.send_modify(|l| l.state = Some(s)),
                    None => warn!("phone link: bad state frame {f:?}"),
                }
            }
            _ = ping.tick() => if answer_by.is_none() {
                write(&mut wr, Cmd::Ping).await?;
                answer_by = Some(tokio::time::Instant::now() + PING_ANSWER);
            },
            _ = kick.notified() => {
                info!("phone link: resumed, checking the link");
                *fast_until = Instant::now() + FAST_WINDOW;
                write(&mut wr, Cmd::Ping).await?;
                let by = tokio::time::Instant::now() + RESUME_ANSWER;
                answer_by = Some(answer_by.map_or(by, |a| a.min(by)));
            }
            _ = async { sleep_until(answer_by.unwrap()).await }, if answer_by.is_some() => {
                bail!("phone didn't answer a ping: link dead");
            }
            Some((cmd, reply)) = cmds.recv() => {
                if reply.is_closed() { continue; } // caller gave up
                let res = write(&mut wr, cmd).await;
                info!("sent {cmd:?}: {res:?}");
                let failed = res.as_ref().err().map(|e| format!("{e:#}"));
                let _ = reply.send(res);
                if let Some(e) = failed { bail!("{e}"); }
            }
        }
    }
}

/// ConnectProfile and take the socket BlueZ hands to our profile meanwhile.
async fn connect(
    device: &bluer::Device,
    profile: &mut ProfileHandle,
    addr: Address,
) -> Result<Stream> {
    let connect = device.connect_profile(&proto::SERVICE);
    pin_mut!(connect);
    let mut connected = false;
    let mut stream = None;
    loop {
        tokio::select! {
            r = &mut connect, if !connected => {
                r.map_err(connect_error)?;
                connected = true;
            }
            req = profile.next(), if stream.is_none() => {
                let req = req.context("bluetoothd dropped our profile")?;
                if req.device() != addr {
                    info!("phone link: rejecting RFCOMM connection from {}", req.device());
                    req.reject(ReqError::Rejected);
                    continue;
                }
                stream = Some(req.accept().context("accept RFCOMM connection")?);
            }
        }
        if connected && let Some(s) = stream.take() {
            return Ok(s);
        }
    }
}

/// ConnectProfile's error, sorted into what the user can do about it. BlueZ's messages differ per
/// version ("br-connection-refused" vs "Connection refused"), hence the loose matching.
fn connect_error(e: bluer::Error) -> anyhow::Error {
    let msg = e.message.to_lowercase();
    if matches!(
        e.kind,
        ErrorKind::AuthenticationFailed | ErrorKind::AuthenticationRejected
    ) || msg.contains("key-missing")
        || msg.contains("key missing")
    {
        return Diagnosed {
            problem: Problem::NotPaired,
            msg: format!(
                "connect: {e}: bond lost? remove the phone in the Bluetooth menu and pair it again"
            ),
        }
        .into();
    }
    // Paged fine, but SDP has no record of our service or nobody listens on its channel.
    if e.kind == ErrorKind::NotAvailable
        || msg.contains("profile-unavailable")
        || msg.contains("refused")
    {
        return Diagnosed {
            problem: Problem::ServiceMissing,
            msg: format!(
                "connect: {e}: phone in reach but the Workstation Connect app isn't listening \
                 (app running? Bluetooth permission?)"
            ),
        }
        .into();
    }
    anyhow!("connect: {e} (phone in range? Bluetooth on?)")
}

/// Next frame's payload (empty: keepalive). Cancel safe: only the read awaits.
async fn next_frame(rd: &mut OwnedReadHalf, dec: &mut Decoder) -> Result<Vec<u8>> {
    let mut buf = [0u8; 256];
    loop {
        if let Some(f) = dec.next_frame() {
            return Ok(f);
        }
        let n = rd.read(&mut buf).await.context("read")?;
        if n == 0 {
            bail!("phone closed the connection");
        }
        dec.feed(&buf[..n]);
    }
}

async fn write(wr: &mut OwnedWriteHalf, cmd: Cmd) -> Result<()> {
    timeout(WRITE, wr.write_all(&cmd.frame()))
        .await
        .map_err(|_| anyhow!("write {cmd:?}: timed out"))?
        .with_context(|| format!("write {cmd:?}"))
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
