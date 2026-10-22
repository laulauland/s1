//! systemd socket activation (`sd_listen_fds(3)`): the daemon serves the
//! listeners systemd passes as fds 3.. when `LISTEN_PID` names this process.
//! systemd keeps them open across restarts, so a client that connects while
//! no daemon runs waits in the backlog for the next one instead of failing.
//!
//! The fds are told apart by socket family: exactly one Unix and one TCP
//! listener (on loopback, as `listen.tcp` requires).

use std::net::TcpListener;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixListener;

/// First fd systemd passes (`SD_LISTEN_FDS_START`).
const FIRST_FD: RawFd = 3;

/// Listeners inherited from systemd.
#[derive(Debug)]
pub struct Inherited {
    pub unix: UnixListener,
    pub tcp: TcpListener,
}

/// How many fds systemd passed to this process, from the values of
/// `LISTEN_PID` and `LISTEN_FDS`. `None` when they are absent or meant for
/// another process (a parent that did not clear them).
pub fn listen_fds(
    pid_var: Option<&str>,
    fds_var: Option<&str>,
    own_pid: u32,
) -> Result<Option<usize>, String> {
    let (Some(pid), Some(fds)) = (pid_var, fds_var) else {
        return Ok(None);
    };
    match pid.trim().parse::<u32>() {
        Ok(p) if p == own_pid => {}
        Ok(_) => return Ok(None),
        Err(_) => return Err(format!("LISTEN_PID `{pid}` is not a pid")),
    }
    let n = fds
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("LISTEN_FDS `{fds}` is not a number"))?;
    Ok((n > 0).then_some(n))
}

/// Take the listeners systemd passed, if any, and clear the `LISTEN_*`
/// variables so child processes do not see them.
///
/// # Safety
///
/// Call before any other thread exists: it changes the process environment
/// and takes ownership of fds 3.. that nothing else in the process may use.
pub unsafe fn take() -> Result<Option<Inherited>, String> {
    let n = listen_fds(
        std::env::var("LISTEN_PID").ok().as_deref(),
        std::env::var("LISTEN_FDS").ok().as_deref(),
        std::process::id(),
    );
    // SAFETY: the caller guarantees no other thread reads the environment.
    unsafe {
        for var in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
            std::env::remove_var(var);
        }
    }
    let Some(n) = n? else {
        return Ok(None);
    };
    let n = RawFd::try_from(n).map_err(|_| format!("LISTEN_FDS {n} is too large"))?;
    let mut unix = None;
    let mut tcp = None;
    let end = FIRST_FD
        .checked_add(n)
        .ok_or_else(|| format!("LISTEN_FDS {n} is too large"))?;
    for fd in FIRST_FD..end {
        // SAFETY: fcntl(2) on an fd number; a closed fd gives EBADF.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(format!(
                "inherited fd {fd}: {}",
                std::io::Error::last_os_error()
            ));
        }
        let family = listening_stream_family(fd).map_err(|e| format!("inherited fd {fd}: {e}"))?;
        match family {
            libc::AF_UNIX if unix.is_none() => {
                // SAFETY: a listening stream socket that systemd gave to this
                // process; nothing else owns it.
                unix = Some(unsafe { UnixListener::from_raw_fd(fd) });
            }
            libc::AF_INET | libc::AF_INET6 if tcp.is_none() => {
                // SAFETY: as above.
                let l = unsafe { TcpListener::from_raw_fd(fd) };
                let addr = l
                    .local_addr()
                    .map_err(|e| format!("inherited fd {fd}: {e}"))?;
                if !addr.ip().is_loopback() {
                    return Err(format!(
                        "inherited TCP listener {addr} is not a loopback address; s1 listens on 127.0.0.1 only"
                    ));
                }
                tcp = Some(l);
            }
            _ => {
                return Err(format!(
                    "inherited fd {fd}: expected one Unix and one TCP listener"
                ));
            }
        }
    }
    match (unix, tcp) {
        (Some(unix), Some(tcp)) => Ok(Some(Inherited { unix, tcp })),
        _ => Err(format!(
            "systemd passed {n} listeners; s1 needs one Unix and one TCP listener"
        )),
    }
}

/// The address family of `fd` if it is a listening stream socket.
fn listening_stream_family(fd: RawFd) -> Result<libc::c_int, String> {
    let int_opt = |opt: libc::c_int| -> Result<libc::c_int, String> {
        let mut v: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: getsockopt(2) writes at most `len` bytes into `v`.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                (&mut v as *mut libc::c_int).cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(v)
    };
    if int_opt(libc::SO_TYPE)? != libc::SOCK_STREAM {
        return Err("not a stream socket".into());
    }
    if int_opt(libc::SO_ACCEPTCONN)? == 0 {
        return Err("not a listening socket".into());
    }
    // SAFETY: an all-zero sockaddr_storage is valid; getsockname(2) writes
    // at most `len` bytes into it.
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockname(
            fd,
            (&mut addr as *mut libc::sockaddr_storage).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(libc::c_int::from(addr.ss_family))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_fds_needs_both_variables_for_this_pid() {
        assert_eq!(listen_fds(None, None, 7), Ok(None));
        assert_eq!(listen_fds(Some("7"), None, 7), Ok(None));
        assert_eq!(listen_fds(None, Some("2"), 7), Ok(None));
        assert_eq!(listen_fds(Some("8"), Some("2"), 7), Ok(None));
        assert_eq!(listen_fds(Some("7"), Some("0"), 7), Ok(None));
        assert_eq!(listen_fds(Some("7"), Some("2"), 7), Ok(Some(2)));
        assert!(listen_fds(Some("x"), Some("2"), 7).is_err());
        assert!(listen_fds(Some("7"), Some("two"), 7).is_err());
    }
}
