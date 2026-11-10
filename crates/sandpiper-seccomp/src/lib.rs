//! Seccomp filters built from a few rule shapes, shared by the VMM sandbox
//! and the guest agent. Linux only; empty elsewhere.
//!
//! Every filter starts the same way: a syscall from a foreign ABI (32-bit
//! compat, x32) kills the process, so `socketcall` and other side doors
//! cannot route around the rules. Anything no rule names is allowed: these
//! are targeted walls, not complete syscall policies.
#![cfg(target_os = "linux")]

use std::io;

pub use libc::sock_filter;

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
const JSET_K: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
const RET_K: u16 = 0x06; // BPF_RET | BPF_K

pub const RET_ALLOW: u32 = 0x7fff_0000;
pub const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;

/// The return value for "fail with this errno".
pub const fn errno(e: i32) -> u32 {
    RET_ERRNO | (e as u32 & 0xffff)
}

/// Every namespace flag `clone` accepts.
pub const NAMESPACE_FLAGS: u32 = (libc::CLONE_NEWNS
    | libc::CLONE_NEWUSER
    | libc::CLONE_NEWNET
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWCGROUP
    | libc::CLONE_NEWTIME) as u32;

fn ins(code: u16, k: u32, jt: u8, jf: u8) -> sock_filter {
    sock_filter { code, jt, jf, k }
}

/// A filter under construction. The accumulator holds the syscall number
/// between rules.
pub struct Filter(Vec<sock_filter>);

impl Default for Filter {
    fn default() -> Self {
        Self::new()
    }
}

impl Filter {
    pub fn new() -> Self {
        let mut f = vec![
            ins(LD_W_ABS, OFF_ARCH, 0, 0),
            ins(JEQ_K, AUDIT_ARCH, 1, 0),
            ins(RET_K, RET_KILL_PROCESS, 0, 0),
            ins(LD_W_ABS, OFF_NR, 0, 0),
        ];
        if cfg!(target_arch = "x86_64") {
            f.push(ins(JGE_K, 0x4000_0000, 0, 1)); // __X32_SYSCALL_BIT
            f.push(ins(RET_K, RET_KILL_PROCESS, 0, 0));
        }
        Self(f)
    }

    /// `nr` always returns `ret`.
    pub fn deny(mut self, nr: libc::c_long, ret: u32) -> Self {
        self.0.push(ins(JEQ_K, nr as u32, 0, 1));
        self.0.push(ins(RET_K, ret, 0, 0));
        self
    }

    /// `nr` returns `ret` when its first argument equals `value`.
    pub fn deny_arg0_eq(self, nr: libc::c_long, value: u32, ret: u32) -> Self {
        self.arg0_rule(nr, JEQ_K, value, ret)
    }

    /// `nr` returns `ret` when its first argument has any bit of `mask`.
    pub fn deny_arg0_any(self, nr: libc::c_long, mask: u32, ret: u32) -> Self {
        self.arg0_rule(nr, JSET_K, mask, ret)
    }

    fn arg0_rule(mut self, nr: libc::c_long, test: u16, k: u32, ret: u32) -> Self {
        self.0.extend([
            ins(JEQ_K, nr as u32, 0, 4), // not this syscall: skip the block
            ins(LD_W_ABS, OFF_ARG0, 0, 0),
            ins(test, k, 0, 1),
            ins(RET_K, ret, 0, 0),
            ins(LD_W_ABS, OFF_NR, 0, 0), // put the number back
        ]);
        self
    }

    /// Refuse every way to make a namespace: `unshare`, `setns`, `clone`
    /// with a namespace flag, and `clone3` (whose flags sit behind a
    /// pointer a filter cannot read; libc falls back to `clone` on ENOSYS).
    pub fn deny_namespaces(self) -> Self {
        self.deny(libc::SYS_unshare, errno(libc::EPERM))
            .deny(libc::SYS_setns, errno(libc::EPERM))
            .deny(libc::SYS_clone3, errno(libc::ENOSYS))
            .deny_arg0_any(libc::SYS_clone, NAMESPACE_FLAGS, errno(libc::EPERM))
    }

    pub fn build(mut self) -> Vec<sock_filter> {
        self.0.push(ins(RET_K, RET_ALLOW, 0, 0));
        self.0
    }
}

/// Install a filter on the calling thread, or with `all_threads` on every
/// thread of the process, after setting `NO_NEW_PRIVS`. Fails, and installs
/// nothing, if any thread cannot take the filter.
pub fn install(filter: &[sock_filter], all_threads: bool) -> io::Result<()> {
    let prog = libc::sock_fprog {
        len: u16::try_from(filter.len()).map_err(|_| io::Error::other("filter too long"))?,
        filter: filter.as_ptr().cast_mut(),
    };
    let flags = if all_threads {
        // With TSYNC alone a failure returns the failing thread's id (> 0);
        // TSYNC_ESRCH makes it an ordinary -1.
        libc::SECCOMP_FILTER_FLAG_TSYNC | libc::SECCOMP_FILTER_FLAG_TSYNC_ESRCH
    } else {
        0
    };
    // SAFETY: valid arguments; the kernel copies the program. Only prctl and
    // syscall are called, so this is safe between fork and exec.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        let rc = libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            flags,
            &prog as *const libc::sock_fprog,
        );
        if rc != 0 {
            return Err(if rc < 0 {
                io::Error::last_os_error()
            } else {
                io::Error::other(format!("seccomp: thread {rc} could not take the filter"))
            });
        }
    }
    Ok(())
}

/// Run a filter over one syscall the way the kernel would, for tests.
pub fn simulate(filter: &[sock_filter], arch: Option<u32>, nr: libc::c_long, arg0: u64) -> u32 {
    let arch = arch.unwrap_or(AUDIT_ARCH);
    let (mut acc, mut pc) = (0u32, 0usize);
    loop {
        let i = filter[pc];
        pc += 1;
        match i.code {
            LD_W_ABS => {
                acc = match i.k {
                    OFF_NR => nr as u32,
                    OFF_ARCH => arch,
                    OFF_ARG0 => arg0 as u32,
                    k => panic!("unexpected offset {k}"),
                }
            }
            RET_K => return i.k,
            JEQ_K | JGE_K | JSET_K => {
                let hit = match i.code {
                    JEQ_K => acc == i.k,
                    JGE_K => acc >= i.k,
                    _ => acc & i.k != 0,
                };
                pc += usize::from(if hit { i.jt } else { i.jf });
            }
            c => panic!("unexpected opcode {c:#x}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<sock_filter> {
        Filter::new()
            .deny(libc::SYS_ptrace, errno(libc::EPERM))
            .deny_arg0_eq(
                libc::SYS_socket,
                libc::AF_VSOCK as u32,
                errno(libc::EAFNOSUPPORT),
            )
            .deny_namespaces()
            .build()
    }

    #[test]
    fn rules_match_only_what_they_name() {
        let f = sample();
        let s = |nr, a| simulate(&f, None, nr, a);
        assert_eq!(s(libc::SYS_ptrace, 0), errno(libc::EPERM));
        assert_eq!(
            s(libc::SYS_socket, libc::AF_VSOCK as u64),
            errno(libc::EAFNOSUPPORT)
        );
        assert_eq!(s(libc::SYS_socket, libc::AF_INET as u64), RET_ALLOW);
        assert_eq!(s(libc::SYS_unshare, 0), errno(libc::EPERM));
        assert_eq!(s(libc::SYS_setns, 0), errno(libc::EPERM));
        assert_eq!(s(libc::SYS_clone3, 0), errno(libc::ENOSYS));
        assert_eq!(
            s(libc::SYS_clone, libc::CLONE_NEWUSER as u64),
            errno(libc::EPERM)
        );
        assert_eq!(
            s(
                libc::SYS_clone,
                (libc::CLONE_NEWNET | libc::CLONE_THREAD) as u64
            ),
            errno(libc::EPERM)
        );
        // An ordinary thread or fork stays allowed.
        let thread = libc::CLONE_VM
            | libc::CLONE_FS
            | libc::CLONE_FILES
            | libc::CLONE_SIGHAND
            | libc::CLONE_THREAD
            | libc::CLONE_SYSVSEM;
        assert_eq!(s(libc::SYS_clone, thread as u64), RET_ALLOW);
        assert_eq!(s(libc::SYS_clone, libc::SIGCHLD as u64), RET_ALLOW);
        assert_eq!(s(libc::SYS_read, 0), RET_ALLOW);
    }

    #[test]
    fn a_foreign_abi_is_killed() {
        let f = sample();
        assert_eq!(simulate(&f, Some(0x4000_0003), 0, 0), RET_KILL_PROCESS); // i386
        if cfg!(target_arch = "x86_64") {
            assert_eq!(simulate(&f, None, 0x4000_0029, 0), RET_KILL_PROCESS); // x32
        }
    }

    #[test]
    fn jumps_stay_in_bounds() {
        let f = sample();
        for (i, x) in f.iter().enumerate() {
            if matches!(x.code, JEQ_K | JGE_K | JSET_K) {
                assert!(i + 1 + usize::from(x.jt.max(x.jf)) < f.len());
            }
        }
        assert_eq!(f.last().unwrap().code, RET_K);
    }
}
