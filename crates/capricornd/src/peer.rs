//! Who is on the other end of a Unix socket.
//!
//! The executable check reads `/proc/<pid>/exe`, which the kernel only allows for
//! processes the daemon may ptrace. In production the daemon runs as its own uid and
//! its clients as the desktop user, so that read fails with EACCES and every peer would
//! be rejected; deployments that separate the uids leave `capture_clients` and
//! `ui_clients` empty and rely on `allowed_uids` plus the socket's group instead. The
//! check is meant for same-uid development setups.

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// `SO_PEERPIDFD` from asm-generic (Linux 6.5+), not yet exported by the libc crate.
const SO_PEERPIDFD: libc::c_int = 77;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    pub pid: i32,
    pub uid: u32,
}

/// Fails unless the peer's uid is in `allowed_uids` and its executable is in
/// `allowed_exes`. An empty list skips that check.
pub fn check_peer(
    stream: &impl AsFd,
    allowed_uids: &[u32],
    allowed_exes: &[PathBuf],
) -> Result<()> {
    let fd = stream.as_fd();
    let cred = peer_cred(fd).context("reading peer credentials")?;
    if !allowed_uids.is_empty() && !allowed_uids.contains(&cred.uid) {
        bail!("peer uid {} is not allowed", cred.uid);
    }
    if allowed_exes.is_empty() {
        return Ok(());
    }
    let exe = peer_exe(fd, cred.pid).with_context(|| {
        format!(
            "cannot read the executable of peer pid {} (a different uid needs ptrace access)",
            cred.pid
        )
    })?;
    if !allowed_exes.contains(&exe) {
        bail!("peer executable {} is not allowed", exe.display());
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

/// Prefers a pidfd, which pins the peer process, over the pid from `SO_PEERCRED`, which
/// may already belong to another process.
fn peer_exe(fd: BorrowedFd<'_>, cred_pid: i32) -> io::Result<PathBuf> {
    let Ok(pidfd) = peer_pidfd(fd) else {
        return exe_of(cred_pid);
    };
    let pid = pid_of_pidfd(&pidfd)?;
    let exe = exe_of(pid)?;
    ensure_alive(&pidfd)?;
    Ok(exe)
}

fn peer_pidfd(fd: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    let mut raw: libc::c_int = -1;
    get_sockopt(fd, SO_PEERPIDFD, &mut raw)?;
    if raw < 0 {
        return Err(io::ErrorKind::NotFound.into());
    }
    // SAFETY: the kernel just handed us this descriptor and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn pid_of_pidfd(pidfd: &OwnedFd) -> io::Result<i32> {
    let path = format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd());
    let pid = BufReader::new(File::open(path)?)
        .lines()
        .map_while(Result::ok)
        .find_map(|line| line.strip_prefix("Pid:").map(|v| v.trim().parse::<i32>()))
        .ok_or(io::ErrorKind::NotFound)?
        .map_err(|_| io::ErrorKind::InvalidData)?;
    if pid <= 0 {
        return Err(io::Error::other("peer process has exited"));
    }
    Ok(pid)
}

fn exe_of(pid: i32) -> io::Result<PathBuf> {
    std::fs::read_link(Path::new("/proc").join(pid.to_string()).join("exe"))
}

/// Signal 0 through the pidfd fails once the pinned process is gone, so a pid reused
/// between reading fdinfo and `exe` is caught.
fn ensure_alive(pidfd: &OwnedFd) -> io::Result<()> {
    // SAFETY: plain syscall on a valid pidfd with a null siginfo.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            0,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
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
        check_peer(&a, &[], &[]).unwrap();
        check_peer(&a, &[my_uid()], &[]).unwrap();
        assert!(check_peer(&a, &[my_uid().wrapping_add(1)], &[]).is_err());
    }

    #[test]
    fn exe_list_is_enforced() {
        let (a, _b) = UnixStream::pair().unwrap();
        let me = std::fs::read_link("/proc/self/exe").unwrap();
        check_peer(&a, &[], &[me]).unwrap();
        let other = PathBuf::from("/nix/store/not-capricorn/bin/capricorn-capture");
        assert!(check_peer(&a, &[], &[other]).is_err());
    }
}
