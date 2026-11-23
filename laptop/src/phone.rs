//! Classic Bluetooth link to the phone: keeps an RFCOMM socket open while awake (like earbuds),
//! streams its state, sends commands.
//!
//! Connect: our own SDP lookup of the service's RFCOMM channel (pages the phone; see sdp.rs for
//! why not BlueZ ConnectProfile), then a plain RFCOMM socket to it with auth + encryption. The
//! channel is cached until a connect to it fails.

use crate::proto::{self, Cmd, Decoder, PhoneState};
use crate::sdp;
use anyhow::{Context, Result, anyhow, bail};
use bluer::rfcomm::stream::{OwnedReadHalf, OwnedWriteHalf};
use bluer::rfcomm::{Security, SecurityLevel, Socket, SocketAddr, Stream};
use bluer::{Address, ErrorKind, Session};
use serde::Serialize;
use std::os::fd::AsRawFd;
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
/// SDP lookup, including paging the phone.
const SDP: Duration = Duration::from_secs(20);
/// RFCOMM connect, including paging (cached channel) and link authentication.
const CONNECT: Duration = Duration::from_secs(20);
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
    // Phone's RFCOMM channel from the last SDP lookup; None: look it up.
    let mut channel = None;
    loop {
        let started = Instant::now();
        let res = attempt(addr, &mut channel, &mut cmds, &link, &kick, &mut fast_until).await;
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
    channel: &mut Option<u8>,
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
    drop((session, adapter, device));

    info!("phone link: connecting to {addr}");
    let connecting = Instant::now();
    let ch = match *channel {
        Some(ch) => {
            info!("phone link: cached RFCOMM channel {ch}, skipping SDP");
            ch
        }
        None => {
            let t = Instant::now();
            let ch = timeout(SDP, sdp::rfcomm_channel(addr, proto::SERVICE))
                .await
                .map_err(|_| anyhow!("SDP: no answer after {}s{UNREACHABLE}", SDP.as_secs()))?
                .map_err(sdp_error)?;
            info!(
                "phone link: SDP: RFCOMM channel {ch} in {} ms",
                t.elapsed().as_millis()
            );
            ch
        }
    };
    // A failed connect may mean the app now listens elsewhere: look it up again next time.
    *channel = None;
    let t = Instant::now();
    let stream = timeout(CONNECT, connect(addr, ch))
        .await
        .map_err(|_| {
            anyhow!(
                "RFCOMM connect: no answer after {}s{UNREACHABLE}",
                CONNECT.as_secs()
            )
        })?
        .map_err(connect_error)?;
    *channel = Some(ch);
    info!(
        "phone link: RFCOMM connected (channel {ch}) in {} ms",
        t.elapsed().as_millis()
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
        "phone connected: first state {} ms after RFCOMM connect ({} ms since connect start, {} ms \
         since attempt start): {state:?}",
        t.elapsed().as_millis(),
        connecting.elapsed().as_millis(),
        started.elapsed().as_millis()
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

const UNREACHABLE: &str = " (phone in range? Bluetooth on?)";
const NOT_LISTENING: &str = "phone in reach but the Workstation Connect app isn't listening (app running? Bluetooth permission?)";

/// RFCOMM socket to the phone. Medium = authenticated (bonded) + encrypted link, which Android's
/// secure listenUsingRfcommWithServiceRecord requires; High would also demand an MITM-protected
/// key and fail on a Just Works bond.
async fn connect(addr: Address, ch: u8) -> std::io::Result<Stream> {
    let sock = Socket::new()?;
    sock.set_security(Security {
        level: SecurityLevel::Medium,
        key_size: 0,
    })?;
    let stream = sock.connect(SocketAddr::new(addr, ch)).await?;
    crate::btsock::settled(stream.as_raw_fd()).await?;
    Ok(stream)
}

/// SDP lookup's error: no record = app not listening; socket errors: most likely not reachable.
fn sdp_error(e: anyhow::Error) -> anyhow::Error {
    if e.is::<sdp::ServiceNotFound>() {
        return Diagnosed {
            problem: Problem::ServiceMissing,
            msg: format!("SDP: {e}: {NOT_LISTENING}"),
        }
        .into();
    }
    if e.chain().any(|c| c.is::<std::io::Error>()) {
        return anyhow!("{e:#}{UNREACHABLE}");
    }
    e
}

/// RFCOMM connect's error, sorted into what the user can do about it.
fn connect_error(e: std::io::Error) -> anyhow::Error {
    /// Linux EACCES: what the kernel makes of HCI "authentication failure" / "key missing".
    const EACCES: i32 = 13;
    if e.raw_os_error() == Some(EACCES) {
        return Diagnosed {
            problem: Problem::NotPaired,
            msg: format!(
                "RFCOMM connect: {e}: bond lost? remove the phone in the Bluetooth menu and pair \
                 it again"
            ),
        }
        .into();
    }
    // Paged fine, but nobody listens on the channel (app stopped, or moved to another channel).
    if e.kind() == std::io::ErrorKind::ConnectionRefused {
        return Diagnosed {
            problem: Problem::ServiceMissing,
            msg: format!("RFCOMM connect: {e}: {NOT_LISTENING}"),
        }
        .into();
    }
    anyhow!("RFCOMM connect: {e}{UNREACHABLE}")
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
