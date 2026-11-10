//! The in-guest part of the first wall (DESIGN.md §4.4): every workload runs
//! as an unprivileged uid, with no new privileges, under a seccomp filter
//! that refuses vsock sockets, io_uring, namespaces and tracing. Root inside
//! the guest is kept for the guest agent.
//!
//! Applied in the child between fork and exec. If any step fails the child
//! never execs, so a guest kernel without seccomp fails closed.

use std::io;

use sandpiper_seccomp::{Filter, errno};

/// The uid and gid every workload runs as. Templates must not give this uid
/// anything a workload should not have.
pub const WORKLOAD_UID: u32 = 1000;
pub const WORKLOAD_GID: u32 = 1000;

/// The workload filter.
///
/// - `socket(AF_VSOCK, …)` fails with `EAFNOSUPPORT`: vsock is the host
///   channel, reserved for the agent;
/// - `io_uring_setup` fails with `ENOSYS`, because io_uring can open sockets
///   without passing through the `socket` syscall;
/// - no namespaces: a user namespace would hand a workload capabilities
///   over new network and mount namespaces, and with them a large kernel
///   surface (netfilter, netlink) to climb to guest root from;
/// - no `ptrace` or `process_vm_*`. This narrows, but does not stop, a
///   workload left running by one exec from reaching a later one: they
///   share a uid, so `/proc/<pid>/mem`, `pidfd_getfd` and signals still
///   work. Work at different trust levels runs in separate machines
///   (DESIGN.md §7.2).
///
/// Everything else is allowed; M2's in-guest confinement narrows further.
pub fn workload_filter() -> Vec<libc::sock_filter> {
    Filter::new()
        .deny_arg0_eq(
            libc::SYS_socket,
            libc::AF_VSOCK as u32,
            errno(libc::EAFNOSUPPORT),
        )
        .deny(libc::SYS_io_uring_setup, errno(libc::ENOSYS))
        .deny_namespaces()
        .deny(libc::SYS_ptrace, errno(libc::EPERM))
        .deny(libc::SYS_process_vm_readv, errno(libc::EPERM))
        .deny(libc::SYS_process_vm_writev, errno(libc::EPERM))
        .build()
}

/// Runs in the forked child after the uid change, before exec. Only
/// async-signal-safe calls (prctl, syscall).
pub fn apply(filter: &[libc::sock_filter]) -> io::Result<()> {
    sandpiper_seccomp::install(filter, false)
}

/// One inside probe, run as a workload. Prints `blocked` and exits 0 if the
/// wall held, prints `open` and exits 1 if it did not, exits 2 on misuse.
pub fn probe(what: &str) -> i32 {
    // SAFETY: plain syscalls; a returned fd is closed at once, and a child
    // made by the userns probe exits at once.
    let blocked = unsafe {
        match what {
            "vsock" => {
                let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
                if fd >= 0 {
                    libc::close(fd);
                }
                fd < 0
            }
            "io_uring" => {
                let mut params = [0u8; 120]; // struct io_uring_params
                let fd = libc::syscall(libc::SYS_io_uring_setup, 1, params.as_mut_ptr());
                if fd >= 0 {
                    libc::close(fd as i32);
                }
                fd < 0
            }
            "userns" => libc::unshare(libc::CLONE_NEWUSER) != 0,
            "clone-userns" => {
                let pid = libc::syscall(
                    libc::SYS_clone,
                    (libc::CLONE_NEWUSER | libc::SIGCHLD) as libc::c_ulong,
                    0usize,
                    0usize,
                    0usize,
                    0usize,
                );
                if pid == 0 {
                    libc::_exit(0);
                }
                if pid > 0 {
                    libc::waitpid(pid as i32, std::ptr::null_mut(), 0);
                }
                pid < 0
            }
            "ptrace" => libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) != 0,
            "root" => libc::geteuid() != 0,
            _ => {
                eprintln!(
                    "unknown probe {what:?}; known: vsock, io_uring, userns, clone-userns, ptrace, root"
                );
                return 2;
            }
        }
    };
    println!("{}", if blocked { "blocked" } else { "open" });
    if blocked { 0 } else { 1 }
}

/// The inside half of the host's wall check (DESIGN.md §4.2), run as a
/// workload before any other. One line per wall:
///
/// - `vsock`: a vsock socket must be refused;
/// - `net-devices`: the guest must have no network interface backed by a
///   device (such as virtio-net). Software-only interfaces with nowhere to
///   send to, such as `lo` and the kernel's `dummy0`, do not count. If the
///   list cannot be read, the wall counts as open;
/// - `canary ADDR`: a TCP connection to each of the host's canaries. The
///   host judges these by whether its listener was reached, not by what
///   this prints.
///
/// Exits 0 only if every line says `blocked`.
pub fn probe_walls(canaries: &[String]) -> i32 {
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;
    let mut all = true;
    let mut line = |name: &str, blocked: bool| {
        println!("{name} {}", if blocked { "blocked" } else { "open" });
        all &= blocked;
    };
    line("vsock", probe_quiet("vsock"));
    let backed = std::fs::read_dir("/sys/class/net").map(|dir| {
        dir.flatten()
            .any(|iface| iface.path().join("device").exists())
    });
    line("net-devices", matches!(backed, Ok(false)));
    for c in canaries {
        let reached = c
            .parse::<SocketAddr>()
            .is_ok_and(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok());
        line(&format!("canary {c}"), !reached);
    }
    if all { 0 } else { 1 }
}

/// [`probe`] without its output: whether the wall held.
fn probe_quiet(what: &str) -> bool {
    // SAFETY: as in `probe`.
    unsafe {
        match what {
            "vsock" => {
                let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
                if fd >= 0 {
                    libc::close(fd);
                }
                fd < 0
            }
            _ => false,
        }
    }
}

/// Mix bytes from the host-backed hardware RNG (virtio-rng) into the kernel
/// pool before any workload runs, so machines booted from one template or
/// one disk branch never share kernel random state (DESIGN.md §4.4). Fails
/// if the device is missing: a guest without it gets no workloads.
pub fn mix_host_entropy() -> io::Result<()> {
    use std::io::{Read, Write};
    let mut seed = [0u8; 64];
    std::fs::File::open("/dev/hwrng")
        .and_then(|mut f| f.read_exact(&mut seed))
        .map_err(|e| io::Error::other(format!("no host entropy from /dev/hwrng: {e}")))?;
    std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/urandom")?
        .write_all(&seed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandpiper_seccomp::{RET_ALLOW, simulate};

    #[test]
    fn workload_filter_holds_each_wall() {
        let f = workload_filter();
        let s = |nr, a| simulate(&f, None, nr, a);
        assert_eq!(
            s(libc::SYS_socket, libc::AF_VSOCK as u64),
            errno(libc::EAFNOSUPPORT)
        );
        assert_eq!(s(libc::SYS_socket, libc::AF_UNIX as u64), RET_ALLOW);
        assert_eq!(s(libc::SYS_io_uring_setup, 0), errno(libc::ENOSYS));
        assert_eq!(s(libc::SYS_unshare, 0), errno(libc::EPERM));
        assert_eq!(
            s(libc::SYS_clone, libc::CLONE_NEWUSER as u64),
            errno(libc::EPERM)
        );
        assert_eq!(s(libc::SYS_clone, libc::SIGCHLD as u64), RET_ALLOW);
        assert_eq!(s(libc::SYS_ptrace, 0), errno(libc::EPERM));
        assert_eq!(s(libc::SYS_openat, 0), RET_ALLOW);
    }
}
