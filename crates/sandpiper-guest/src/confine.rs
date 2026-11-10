//! The in-guest part of the first wall (DESIGN.md §4.4): every workload runs
//! as an unprivileged uid, with no new privileges, under a seccomp filter
//! that refuses vsock sockets and io_uring. Root inside the guest is kept for
//! the guest agent.
//!
//! Applied in the child between fork and exec. If any step fails the child
//! never execs, so a guest kernel without seccomp fails closed.

use std::io;

/// The uid and gid every workload runs as. Templates must not give this uid
/// anything a workload should not have.
pub const WORKLOAD_UID: u32 = 1000;
pub const WORKLOAD_GID: u32 = 1000;

#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7; // AUDIT_ARCH_AARCH64
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E; // AUDIT_ARCH_X86_64

// Offsets into `struct seccomp_data` (little-endian on both targets).
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0: u32 = 16;

// Classic BPF opcodes, spelled out so this does not depend on which ones a
// given libc release exports.
const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
const RET_K: u16 = 0x06; // BPF_RET | BPF_K

const RET_ALLOW: u32 = 0x7fff_0000;
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// The workload filter. Anything not named here is allowed: this is one
/// targeted wall, not a full syscall policy (that is M2's in-guest
/// confinement).
///
/// - a foreign syscall ABI (32-bit compat, x32) kills the process, so
///   `socketcall` and friends cannot route around the checks below;
/// - `socket(AF_VSOCK, …)` fails with `EAFNOSUPPORT`;
/// - `io_uring_setup` fails with `ENOSYS`, because io_uring can open
///   sockets without passing through the `socket` syscall.
pub fn workload_filter() -> Vec<libc::sock_filter> {
    let mut f = vec![
        stmt(LD_W_ABS, OFF_ARCH),
        jump(JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(RET_K, RET_KILL_PROCESS),
        stmt(LD_W_ABS, OFF_NR),
    ];
    if cfg!(target_arch = "x86_64") {
        f.extend([
            jump(JGE_K, 0x4000_0000, 0, 1), // __X32_SYSCALL_BIT
            stmt(RET_K, RET_KILL_PROCESS),
        ]);
    }
    f.extend([
        jump(JEQ_K, libc::SYS_io_uring_setup as u32, 0, 1),
        stmt(RET_K, RET_ERRNO | libc::ENOSYS as u32),
        jump(JEQ_K, libc::SYS_socket as u32, 0, 3),
        stmt(LD_W_ABS, OFF_ARG0),
        jump(JEQ_K, libc::AF_VSOCK as u32, 0, 1),
        stmt(RET_K, RET_ERRNO | libc::EAFNOSUPPORT as u32),
        stmt(RET_K, RET_ALLOW),
    ]);
    f
}

/// Runs in the forked child after the uid change, before exec. Only
/// async-signal-safe calls: `prctl` and `errno`.
pub fn apply(filter: &[libc::sock_filter]) -> io::Result<()> {
    let prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr().cast_mut(),
    };
    // SAFETY: prctl with valid arguments; `prog` and `filter` outlive the
    // calls, and the kernel copies the program.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &prog as *const libc::sock_fprog,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
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

/// One inside probe, run as a workload. Prints `blocked` and exits 0 if the
/// wall held, prints `open` and exits 1 if it did not, exits 2 on misuse.
pub fn probe(what: &str) -> i32 {
    let blocked = match what {
        // SAFETY: plain syscalls; a returned fd is closed at once.
        "vsock" => unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
            if fd >= 0 {
                libc::close(fd);
            }
            fd < 0
        },
        "io_uring" => unsafe {
            let mut params = [0u8; 120]; // struct io_uring_params
            let fd = libc::syscall(libc::SYS_io_uring_setup, 1, params.as_mut_ptr());
            if fd >= 0 {
                libc::close(fd as i32);
            }
            fd < 0
        },
        "root" => unsafe { libc::geteuid() != 0 },
        _ => {
            eprintln!("unknown probe {what:?}; known: vsock, io_uring, root");
            return 2;
        }
    };
    println!("{}", if blocked { "blocked" } else { "open" });
    if blocked { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the filter over one syscall, the way the kernel would.
    fn eval(filter: &[libc::sock_filter], arch: u32, nr: u32, arg0: u32) -> u32 {
        let load = |off: u32| match off {
            OFF_NR => nr,
            OFF_ARCH => arch,
            OFF_ARG0 => arg0,
            _ => panic!("unexpected offset {off}"),
        };
        let (mut acc, mut pc) = (0u32, 0usize);
        loop {
            let ins = filter[pc];
            pc += 1;
            match ins.code {
                LD_W_ABS => acc = load(ins.k),
                RET_K => return ins.k,
                JEQ_K | JGE_K => {
                    let hit = if ins.code == JEQ_K {
                        acc == ins.k
                    } else {
                        acc >= ins.k
                    };
                    pc += usize::from(if hit { ins.jt } else { ins.jf });
                }
                c => panic!("unexpected opcode {c:#x}"),
            }
        }
    }

    #[test]
    fn filter_refuses_vsock_and_io_uring_and_allows_the_rest() {
        let f = workload_filter();
        let sock = libc::SYS_socket as u32;
        let eaf = RET_ERRNO | libc::EAFNOSUPPORT as u32;
        assert_eq!(eval(&f, AUDIT_ARCH, sock, libc::AF_VSOCK as u32), eaf);
        assert_eq!(eval(&f, AUDIT_ARCH, sock, libc::AF_INET as u32), RET_ALLOW);
        assert_eq!(eval(&f, AUDIT_ARCH, sock, libc::AF_UNIX as u32), RET_ALLOW);
        assert_eq!(
            eval(&f, AUDIT_ARCH, libc::SYS_io_uring_setup as u32, 0),
            RET_ERRNO | libc::ENOSYS as u32
        );
        assert_eq!(eval(&f, AUDIT_ARCH, libc::SYS_openat as u32, 0), RET_ALLOW);
    }

    #[test]
    fn filter_kills_a_foreign_abi() {
        let f = workload_filter();
        assert_eq!(eval(&f, 0x4000_0003, 0, 0), RET_KILL_PROCESS); // AUDIT_ARCH_I386
        if cfg!(target_arch = "x86_64") {
            assert_eq!(eval(&f, AUDIT_ARCH, 0x4000_0029, 0), RET_KILL_PROCESS);
        }
    }

    #[test]
    fn filter_jumps_stay_in_bounds() {
        let f = workload_filter();
        for (i, ins) in f.iter().enumerate() {
            if ins.code == JEQ_K || ins.code == JGE_K {
                assert!(i + 1 + usize::from(ins.jt.max(ins.jf)) < f.len());
            }
        }
        assert_eq!(f.last().unwrap().code, RET_K);
    }
}
