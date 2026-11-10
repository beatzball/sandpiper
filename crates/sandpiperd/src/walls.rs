//! Wall checks (DESIGN.md §4.2), Linux only. Every machine is checked after
//! its guest agent answers and before it is handed out. A failed check
//! stops the machine and marks it `failed`, so a broken host fails loudly
//! instead of quietly running open machines.
//!
//! - **Outside:** the VMM has its own network namespace, with no device but
//!   `lo`; it is in the machine's cgroup; it runs as the machine's uid.
//! - **Inside:** the guest agent runs `probe walls` as a workload: a vsock
//!   socket must be refused, the guest must have no network interface but
//!   `lo`, and it tries to connect to each of the host's canaries.
//! - **Canaries** are TCP listeners the host makes for this one check: on
//!   the host's loopback, on its LAN address, and inside the VMM's own
//!   network namespace (which only TSI could reach). A canary counts as
//!   reached if its listener has a connection waiting, judged here, never
//!   from what the guest prints.

use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::thread;

/// The address the canary inside the VMM's namespace listens on. From a
/// documentation range, so it means nothing anywhere else.
const NETNS_CANARY: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);

/// The VMM process a supervisor started: its only child.
pub fn vmm_of(supervisor: u32) -> io::Result<u32> {
    let children = fs::read_to_string(format!("/proc/{supervisor}/task/{supervisor}/children"))?;
    children
        .split_whitespace()
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| io::Error::other("the supervisor has no VMM"))
}

/// The outside checks. Returns the names of the checks that passed, or why
/// one failed.
pub fn outside(vmm: u32, machine: &str, uid: u32) -> Result<Vec<&'static str>, String> {
    let proc = format!("/proc/{vmm}");
    let read = |rest: &str| {
        fs::read_to_string(format!("{proc}/{rest}")).map_err(|e| format!("read {rest}: {e}"))
    };
    let link = |p: &str| fs::read_link(p).map_err(|e| format!("read {p}: {e}"));

    if link(&format!("{proc}/ns/net"))? == link("/proc/self/ns/net")? {
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
    Ok(vec!["netns", "netns-devices", "cgroup", "uid"])
}

pub struct Canaries {
    list: Vec<(&'static str, TcpListener)>,
}

impl Canaries {
    /// Open the canaries for one check of the VMM `vmm`.
    pub fn open(vmm: u32) -> io::Result<Self> {
        let mut list = vec![("host-loopback", listen(Ipv4Addr::LOCALHOST)?)];
        if let Some(lan) = lan_address() {
            list.push(("host-lan", listen(lan)?));
        }
        list.push(("vmm-netns", listen_in_netns(vmm)?));
        Ok(Self { list })
    }

    /// The addresses for the guest to try, as `ADDR:PORT`.
    pub fn addrs(&self) -> Vec<String> {
        self.list
            .iter()
            .filter_map(|(_, l)| l.local_addr().ok())
            .map(|a| a.to_string())
            .collect()
    }

    /// The names of the canaries, for the record.
    pub fn names(&self) -> Vec<&'static str> {
        self.list.iter().map(|(n, _)| *n).collect()
    }

    /// The first canary with a connection waiting, if any.
    pub fn reached(&self) -> Option<&'static str> {
        self.list
            .iter()
            .find(|(_, l)| l.accept().is_ok())
            .map(|(name, _)| *name)
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

/// A listener inside the VMM's network namespace, on [`NETNS_CANARY`]. Made
/// on a thread of its own, which enters the namespace and then exits; the
/// socket stays in the namespace.
fn listen_in_netns(vmm: u32) -> io::Result<TcpListener> {
    let ns = fs::File::open(Path::new(&format!("/proc/{vmm}/ns/net")))?;
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
        let close = |r: io::Result<()>| {
            libc::close(s);
            r
        };
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
        if libc::ioctl(s, libc::SIOCSIFADDR, &req) < 0 {
            return close(Err(io::Error::last_os_error()));
        }
        req.ifr_ifru.ifru_flags = (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        if libc::ioctl(s, libc::SIOCSIFFLAGS, &req) < 0 {
            return close(Err(io::Error::last_os_error()));
        }
        close(Ok(()))
    }
}
