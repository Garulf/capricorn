//! Who is on the other end of a Unix socket.
//!
//! Only the uid is checked. The daemon runs as its own uid, so it cannot read a
//! client's `/proc/<pid>/exe`; the socket's group and `allowed_uids` decide who connects.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    pub pid: i32,
    pub uid: u32,
}

/// Fails unless the peer's uid is in `allowed_uids`. An empty list skips the check.
pub fn check_peer(stream: &impl AsFd, allowed_uids: &[u32]) -> Result<()> {
    let cred = peer_cred(stream.as_fd()).context("reading peer credentials")?;
    if !allowed_uids.is_empty() && !allowed_uids.contains(&cred.uid) {
        bail!("peer uid {} is not allowed", cred.uid);
    }
    Ok(())
}

pub fn peer_cred(fd: BorrowedFd<'_>) -> io::Result<PeerCred> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    get_sockopt(fd, libc::SO_PEERCRED, &mut cred)?;
    Ok(PeerCred {
        pid: cred.pid,
        uid: cred.uid,
    })
}

fn get_sockopt<T>(fd: BorrowedFd<'_>, option: libc::c_int, value: &mut T) -> io::Result<()> {
    let mut len = std::mem::size_of::<T>() as libc::socklen_t;
    // SAFETY: `value` is a valid, writable `T` and `len` is its exact size.
    let ret = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (value as *mut T).cast(),
            &mut len,
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of::<T>() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::*;

    fn my_uid() -> u32 {
        // SAFETY: getuid has no failure modes.
        unsafe { libc::getuid() }
    }

    #[test]
    fn peer_cred_reports_this_process() {
        let (a, _b) = UnixStream::pair().unwrap();
        let cred = peer_cred(a.as_fd()).unwrap();
        assert_eq!(cred.uid, my_uid());
        assert_eq!(cred.pid, std::process::id() as i32);
    }

    #[test]
    fn uid_list_is_enforced_and_empty_allows_all() {
        let (a, _b) = UnixStream::pair().unwrap();
        check_peer(&a, &[]).unwrap();
        check_peer(&a, &[my_uid()]).unwrap();
        assert!(check_peer(&a, &[my_uid().wrapping_add(1)]).is_err());
    }
}
