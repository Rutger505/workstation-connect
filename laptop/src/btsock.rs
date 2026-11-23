//! Workaround for bluer's non-blocking connect (0.17): the socket is registered with tokio while
//! still unconnected, when the kernel reports it writable. That readiness is still cached when
//! connect() returns EINPROGRESS, so bluer's connect resolves at once with the link still being
//! set up (SO_ERROR 0) and the first send fails with ENOTCONN. [`settled`] waits for the real
//! outcome with level-triggered poll(2), which the kernel answers without POLLOUT while a
//! Bluetooth socket is connecting.

use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

const STEP: Duration = Duration::from_millis(20);

/// Waits until a just-connected Bluetooth socket is really connected; the connect's error if it
/// failed. Not bounded: wrap in a timeout. Cancel-safe (no blocking thread holds the fd).
pub async fn settled(fd: RawFd) -> io::Result<()> {
    loop {
        match check(fd)? {
            Some(()) => return Ok(()),
            None => tokio::time::sleep(STEP).await,
        }
    }
}

/// Some(()) connected, None still connecting, Err failed.
fn check(fd: RawFd) -> io::Result<Option<()>> {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: one valid pollfd, zero timeout; the caller owns fd for the duration of the call.
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted {
            Ok(None)
        } else {
            Err(e)
        };
    }
    if p.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        return Err(so_error(fd)?
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "connection closed")));
    }
    Ok((p.revents & libc::POLLOUT != 0).then_some(()))
}

fn so_error(fd: RawFd) -> io::Result<Option<io::Error>> {
    let mut err: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: err/len point to a c_int and its size, as SO_ERROR expects.
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&mut err as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((err != 0).then(|| io::Error::from_raw_os_error(err)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[tokio::test]
    async fn connected_socket_settles() {
        let (a, _b) = UnixStream::pair().unwrap();
        settled(a.as_raw_fd()).await.unwrap();
    }

    #[tokio::test]
    async fn closed_peer_is_an_error() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(b);
        assert!(settled(a.as_raw_fd()).await.is_err());
    }
}
