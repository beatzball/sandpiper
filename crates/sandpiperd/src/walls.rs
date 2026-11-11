//! Wall checks (DESIGN.md §4.2), Linux only. Every machine is checked after
//! its guest agent answers and before it is handed out. A failed check
//! stops the machine and marks it `failed`, so a broken host fails loudly
//! instead of quietly running open machines.
//!
//! - **Outside, judged by the host:** the VMM has its own network namespace,
//!   with no device but `lo`; it is in the machine's cgroup; it runs as the
//!   machine's uid. These hold whatever the guest does: even with TSI on,
//!   the guest's sockets act inside that empty namespace.
//! - **Canaries, judged by the host:** TCP listeners made for this one
//!   check, on the host's loopback, on its LAN address, and inside the VMM's
//!   own network namespace (which only TSI could reach). The guest probe
//!   connects to each and sends a code made for this check; a canary counts
//!   as reached only if that code arrives, so a stray connection from
//!   elsewhere cannot fail a machine.
//! - **Inside, reported by the guest agent:** a vsock socket is refused, and
//!   no network interface is backed by a device. The probe must finish
//!   cleanly and report every wall, or the check fails. This part trusts
//!   the guest agent, which is the platform's own (DESIGN.md §4.4); the
//!   record names these walls `guest:…`.

use std::fs;
use std::io::{self, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::thread;
use std::time::{Duration, Instant};

/// The address the canary inside the VMM's namespace listens on. From a
/// documentation range, so it means nothing anywhere else.
const NETNS_CANARY: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);

/// How long a canary connection gets to send its code.
const CODE_WAIT: Duration = Duration::from_millis(200);
/// Connections without the code one canary may take before the check fails.
const MAX_STRAYS: usize = 8;
/// How long reading all canaries may take.
const DRAIN_LIMIT: Duration = Duration::from_secs(1);

/// A VMM process, pinned: while its pidfd says it is alive, its pid and
/// `/proc` entries are still its own, and `netns` is its namespace.
pub struct Vmm {
    pid: u32,
    pidfd: OwnedFd,
    netns: fs::File,
}

impl Vmm {
    /// The VMM a supervisor started: its only child.
    pub fn of(supervisor: u32) -> io::Result<Self> {
        let children =
            fs::read_to_string(format!("/proc/{supervisor}/task/{supervisor}/children"))?;
        let pid: u32 = children
            .split_whitespace()
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| io::Error::other("the supervisor has no VMM"))?;
        // SAFETY: pidfd_open returns a new descriptor or -1.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh descriptor we own.
        let pidfd = unsafe { OwnedFd::from_raw_fd(fd) };
        // The pid could have been reused between reading it and pinning it:
        // the pinned process must still be the supervisor's child.
        let parent = fs::read_to_string(format!("/proc/{pid}/status"))?
            .lines()
            .find_map(|l| l.strip_prefix("PPid:"))
            .and_then(|v| v.trim().parse::<u32>().ok());
        if parent != Some(supervisor) {
            return Err(io::Error::other("the VMM's pid now names another process"));
        }
        let netns = fs::File::open(format!("/proc/{pid}/ns/net"))?;
        let vmm = Self { pid, pidfd, netns };
        // Alive now, so the pid was not reused before the pidfd and the
        // namespace were opened.
        vmm.still_alive()?;
        Ok(vmm)
    }

    fn still_alive(&self) -> io::Result<()> {
        // SAFETY: signal 0 through a pidfd we own only checks the process.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                0,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::other("the VMM exited during the wall check"))
        }
    }
}

/// The outside checks. Returns the names of the checks that passed, or why
/// one failed.
pub fn outside(v: &Vmm, machine: &str, uid: u32) -> Result<Vec<&'static str>, String> {
    let proc = format!("/proc/{}", v.pid);
    let read = |rest: &str| {
        fs::read_to_string(format!("{proc}/{rest}")).map_err(|e| format!("read {rest}: {e}"))
    };
    let ns_id = |m: fs::Metadata| (m.dev(), m.ino());
    let theirs = v.netns.metadata().map_err(|e| e.to_string())?;
    let ours = fs::metadata("/proc/self/ns/net").map_err(|e| e.to_string())?;
    if ns_id(theirs) == ns_id(ours) {
        return Err("the VMM shares the host's network namespace".into());
    }
    let devices: Vec<String> = read("net/dev")?
        .lines()
        .skip(2)
        .filter_map(|l| l.split(':').next())
        .map(|d| d.trim().to_owned())
        .filter(|d| d != "lo")
        .collect();
    if !devices.is_empty() {
        return Err(format!(
            "the VMM's network namespace has devices {devices:?}"
        ));
    }
    let cgroup = read("cgroup")?;
    let in_group = cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .is_some_and(|p| p.ends_with(&format!("/{machine}")));
    if !in_group {
        return Err("the VMM is not in the machine's cgroup".into());
    }
    let status = read("status")?;
    let ids_ok = |key: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .is_some_and(|v| v.split_whitespace().all(|id| id == uid.to_string()))
    };
    if !(ids_ok("Uid:") && ids_ok("Gid:")) {
        return Err("the VMM does not run as the machine's uid and gid".into());
    }
    // Everything above was read from the pinned process.
    v.still_alive().map_err(|e| e.to_string())?;
    Ok(vec!["netns", "netns-devices", "cgroup", "uid"])
}

pub struct Canaries {
    list: Vec<(&'static str, TcpListener)>,
    /// What a probe sends on each connection: made for this check only.
    pub code: String,
}

impl Canaries {
    /// Open the canaries for one check of `v`.
    pub fn open(v: &Vmm) -> io::Result<Self> {
        let mut list = vec![("host-loopback", listen(Ipv4Addr::LOCALHOST)?)];
        if let Some(lan) = lan_address() {
            list.push(("host-lan", listen(lan)?));
        }
        list.push(("vmm-netns", listen_in_netns(v.netns.try_clone()?)?));
        Ok(Self {
            list,
            code: crate::ids::random_hex(16),
        })
    }

    /// Each canary's name and the address the guest should try.
    pub fn targets(&self) -> io::Result<Vec<(&'static str, String)>> {
        self.list
            .iter()
            .map(|(name, l)| Ok((*name, l.local_addr()?.to_string())))
            .collect()
    }

    /// The first canary that received this check's code, if any. Other
    /// connections are read and dropped, at most [`MAX_STRAYS`] per canary
    /// and within [`DRAIN_LIMIT`] in all; past either, the check fails
    /// rather than wait. Any error fails the check too: an error must never
    /// read as "not reached".
    pub fn reached(&self) -> io::Result<Option<&'static str>> {
        let deadline = Instant::now() + DRAIN_LIMIT;
        let flooded = || io::Error::other("canaries flooded by other connections");
        for (name, listener) in &self.list {
            let mut strays = 0;
            loop {
                match listener.accept() {
                    Ok((mut conn, _)) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            return Err(flooded());
                        }
                        conn.set_nonblocking(false)?;
                        conn.set_read_timeout(Some(left.min(CODE_WAIT)))?;
                        let mut got = [0u8; 64];
                        let n = conn.read(&mut got).unwrap_or(0);
                        if got[..n]
                            .windows(self.code.len())
                            .any(|w| w == self.code.as_bytes())
                        {
                            return Ok(Some(name));
                        }
                        strays += 1;
                        if strays >= MAX_STRAYS {
                            return Err(flooded());
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(None)
    }
}

fn listen(ip: Ipv4Addr) -> io::Result<TcpListener> {
    let l = TcpListener::bind(SocketAddr::from((ip, 0)))?;
    l.set_nonblocking(true)?;
    Ok(l)
}

/// The host's address on its default route, if it has one. Connecting a
/// UDP socket sends nothing; it only picks the source address.
fn lan_address() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(198, 51, 100, 1), 9)).ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// A listener inside the network namespace `ns`, on [`NETNS_CANARY`]. Made
/// on a thread of its own, which enters the namespace and then exits; the
/// socket stays in the namespace.
fn listen_in_netns(ns: fs::File) -> io::Result<TcpListener> {
    thread::spawn(move || -> io::Result<TcpListener> {
        // SAFETY: setns on this thread only, with a namespace fd we own.
        if unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
            return Err(io::Error::last_os_error());
        }
        lo_up_with(NETNS_CANARY)?;
        listen(NETNS_CANARY)
    })
    .join()
    .map_err(|_| io::Error::other("netns canary thread panicked"))?
}

/// Bring `lo` up in the current network namespace and give it `ip`.
fn lo_up_with(ip: Ipv4Addr) -> io::Result<()> {
    // SAFETY: ioctls on a socket we own, with a zeroed and filled ifreq.
    unsafe {
        let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if s < 0 {
            return Err(io::Error::last_os_error());
        }
        let s = OwnedFd::from_raw_fd(s);
        let mut req: libc::ifreq = std::mem::zeroed();
        for (d, b) in req.ifr_name.iter_mut().zip(b"lo\0") {
            *d = *b as libc::c_char;
        }
        let sin = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: 0,
            sin_addr: libc::in_addr {
                s_addr: u32::from(ip).to_be(),
            },
            sin_zero: [0; 8],
        };
        std::ptr::copy_nonoverlapping(
            (&sin as *const libc::sockaddr_in).cast::<u8>(),
            std::ptr::addr_of_mut!(req.ifr_ifru).cast::<u8>(),
            std::mem::size_of::<libc::sockaddr_in>(),
        );
        if libc::ioctl(s.as_raw_fd(), libc::SIOCSIFADDR, &req) < 0 {
            return Err(io::Error::last_os_error());
        }
        req.ifr_ifru.ifru_flags = (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        if libc::ioctl(s.as_raw_fd(), libc::SIOCSIFFLAGS, &req) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
