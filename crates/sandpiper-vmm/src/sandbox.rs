//! The outer layer of the first wall (DESIGN.md §4.2), Linux only: the VMM
//! process gets its own mount, PID, network, IPC and UTS namespaces, sees
//! only its machine's files and a few device nodes, switches to its own
//! unprivileged uid just before the guest starts, and runs under a seccomp
//! filter that refuses the syscalls a VMM escape would reach for.
//!
//! The process must start as root. Order:
//!
//! 1. [`enter`]: new namespaces and a fork. The child becomes PID 1 of the
//!    new PID namespace and goes on; the parent becomes its **supervisor**
//!    (see [`supervise`]). The child closes every inherited descriptor and
//!    moves into a minimal root built on a tmpfs with `pivot_root`.
//! 2. libkrun is configured; it records the disk and socket paths.
//! 3. [`limit_resources`], [`drop_groups`] and [`install_filter`] run in
//!    `before_enter`.
//! 4. `krun_start_enter` switches to the machine's uid, then opens
//!    `/dev/kvm` (hence the private node owned by that uid) and starts the
//!    guest.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use sandpiper_seccomp::{Filter, errno};

/// Where the machine's directory appears inside the sandbox.
pub const MACHINE_DIR: &str = "/machine";

/// The lowest uid or gid a sandboxed VMM may run as.
pub const MIN_ID: u32 = 100_000;

/// World-usable device nodes, bound into the sandbox from the host.
const DEVICES: &[&str] = &["/dev/null", "/dev/zero", "/dev/urandom"];

/// The descriptor number the child keeps for its end of the parent pipe.
const ALIVE_FD: libc::c_int = 3;

fn os(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

fn c(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::other(format!("NUL byte in {}", p.display())))
}

fn mount(
    src: Option<&Path>,
    dst: &Path,
    fstype: Option<&str>,
    flags: u64,
    data: Option<&str>,
) -> io::Result<()> {
    let src = src.map(c).transpose()?;
    let fstype = fstype.map(|f| CString::new(f).expect("static"));
    let data = data.map(|d| CString::new(d).expect("static"));
    // SAFETY: every pointer is a valid C string or NULL, alive for the call.
    os(unsafe {
        libc::mount(
            src.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            c(dst)?.as_ptr(),
            fstype.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            flags as libc::c_ulong,
            data.as_ref()
                .map_or(std::ptr::null(), |s| s.as_ptr().cast()),
        )
    })
    .map(drop)
    .map_err(|e| io::Error::other(format!("mount {}: {e}", dst.display())))
}

/// Enter the sandbox. `machine` is the host directory holding this machine's
/// disk image and socket; inside, it is the only host directory visible, at
/// [`MACHINE_DIR`]. Returns in the sandboxed child; the parent never returns.
///
/// With `lifeline`, stdin is a pipe from the driver: when it closes, the
/// supervisor kills the machine.
///
/// The returned descriptor hangs up when the supervisor exits. Watching it
/// is a courtesy for a well-behaved VMM; the supervisor's SIGKILL is what
/// does not depend on the VMM's cooperation.
pub fn enter(
    scratch: &Path,
    machine: &Path,
    uid: u32,
    gid: u32,
    lifeline: bool,
) -> io::Result<OwnedFd> {
    // libkrun loads its kernel library by name when the VM starts, which is
    // after the host filesystem is gone. Load it now; libkrun's own dlopen
    // then finds it already in the process.
    // SAFETY: dlopen of a library whose constructors have no preconditions.
    let fw = unsafe {
        libc::dlopen(
            c"libkrunfw.so.5".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_GLOBAL,
        )
    };
    if fw.is_null() {
        return Err(io::Error::other("cannot load libkrunfw.so.5"));
    }
    let mut alive = [0; 2];
    // SAFETY: unshare and fork with no threads running yet (called before
    // any thread is spawned), so the child is a full copy of this process.
    unsafe {
        os(libc::unshare(
            libc::CLONE_NEWNS
                | libc::CLONE_NEWPID
                | libc::CLONE_NEWNET
                | libc::CLONE_NEWIPC
                | libc::CLONE_NEWUTS,
        ))
        .map_err(|e| io::Error::other(format!("unshare: {e}")))?;
        // The supervisor holds the write end of this pipe until it exits.
        // The child cannot use getppid() to see whether the supervisor is
        // still there: across a PID namespace it always reads 0.
        os(libc::pipe2(alive.as_mut_ptr(), libc::O_CLOEXEC))?;
        let pid = os(libc::fork())?;
        if pid > 0 {
            libc::close(alive[0]);
            supervise(pid, lifeline);
        }
        libc::close(alive[1]);
        // Covers the window until libkrun's uid switch, which clears it.
        os(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0))?;
        let mut pfd = libc::pollfd {
            fd: alive[0],
            events: libc::POLLIN,
            revents: 0,
        };
        if libc::poll(&mut pfd, 1, 0) != 0 {
            libc::_exit(125); // the supervisor died before the signal was armed
        }
        // Nothing the caller left open may follow the VMM into the jail: an
        // inherited directory descriptor would reach around pivot_root.
        // Keep stdio and the pipe, at a known number, and close the rest.
        if alive[0] != ALIVE_FD {
            os(libc::dup3(alive[0], ALIVE_FD, libc::O_CLOEXEC))?;
            libc::close(alive[0]);
        }
        os(libc::syscall(libc::SYS_close_range, ALIVE_FD + 1, libc::c_uint::MAX, 0) as libc::c_int)
            .map_err(|e| io::Error::other(format!("close_range: {e}")))?;
    }
    // SAFETY: ALIVE_FD is open and owned by nothing else from here on.
    let alive = unsafe { OwnedFd::from_raw_fd(ALIVE_FD) };

    // Nothing done in here may reach the host's mount table.
    mount(
        None,
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    )?;

    let new_root = scratch.join("root");
    fs::DirBuilder::new().mode(0o700).create(&new_root)?;
    mount(
        Some(Path::new("tmpfs")),
        &new_root,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NOEXEC,
        Some("size=64k,mode=0755"),
    )?;
    fs::create_dir(new_root.join("dev"))?;
    for dev in DEVICES {
        let dst = new_root.join(dev.trim_start_matches('/'));
        fs::File::create(&dst)?;
        mount(Some(Path::new(dev)), &dst, None, libc::MS_BIND, None)?;
    }
    // libkrun opens /dev/kvm after it has switched to the machine's uid, so
    // the sandbox gets its own node for the same device, owned by that uid.
    // The host's node, and its `kvm` group, stay untouched.
    let kvm = fs::metadata("/dev/kvm")?;
    let kvm_node = c(&new_root.join("dev/kvm"))?;
    // SAFETY: mknod and chown on a path in our private tmpfs.
    unsafe {
        use std::os::unix::fs::MetadataExt;
        os(libc::mknod(
            kvm_node.as_ptr(),
            libc::S_IFCHR | 0o600,
            kvm.rdev() as libc::dev_t,
        ))?;
        os(libc::chown(kvm_node.as_ptr(), uid, gid))?;
    }
    let inside = new_root.join(MACHINE_DIR.trim_start_matches('/'));
    fs::create_dir(&inside)?;
    mount(Some(machine), &inside, None, libc::MS_BIND, None)?;
    mount(
        None,
        &inside,
        None,
        libc::MS_REMOUNT | libc::MS_BIND | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    )?;

    // SAFETY: path arguments are valid C strings; pivot_root has no libc
    // wrapper, so it goes through syscall(). It moves the root of every
    // process in this mount namespace, the supervisor's too.
    unsafe {
        let dot = CString::new(".").expect("static");
        os(libc::chdir(c(&new_root)?.as_ptr()))?;
        os(libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), dot.as_ptr()) as libc::c_int)
            .map_err(|e| io::Error::other(format!("pivot_root: {e}")))?;
        os(libc::umount2(dot.as_ptr(), libc::MNT_DETACH))?;
        os(libc::chdir(c(Path::new("/"))?.as_ptr()))?;
    }
    // The tmpfs root holds only mount points from here on.
    mount(
        None,
        Path::new("/"),
        None,
        libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NOEXEC,
        None,
    )?;
    Ok(alive)
}

/// The supervisor: stays root and outside the PID namespace (it shares the
/// others, and the jail's root), and kills the machine — the whole PID
/// namespace, by SIGKILL to its init — when the driver says so or is gone:
///
/// - SIGTERM, SIGINT or SIGHUP;
/// - the driver's death (`PR_SET_PDEATHSIG`, delivered as SIGTERM; the
///   supervisor never changes uid, so the kernel keeps it armed);
/// - with `lifeline`, end of stdin.
///
/// None of this needs the VMM's cooperation. Exits with the VMM's status.
fn supervise(child: libc::pid_t, lifeline: bool) -> ! {
    // SAFETY: signal-mask, signalfd, pidfd and poll calls on descriptors
    // made here; the process is single-threaded.
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut mask, s);
        }
        libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
        let sfd = libc::signalfd(-1, &mask, libc::SFD_CLOEXEC);
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0);
        let pidfd = libc::syscall(libc::SYS_pidfd_open, child, 0) as libc::c_int;
        if sfd < 0 || pidfd < 0 {
            libc::kill(child, libc::SIGKILL);
        } else {
            let mut fds = [
                libc::pollfd {
                    fd: pidfd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: sfd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if lifeline { 0 } else { -1 },
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            loop {
                if libc::poll(fds.as_mut_ptr(), 3, -1) < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    libc::kill(child, libc::SIGKILL);
                    break;
                }
                if fds[0].revents != 0 {
                    break; // the VMM exited by itself
                }
                if fds[1].revents != 0 {
                    libc::kill(child, libc::SIGKILL);
                    break;
                }
                if fds[2].revents != 0 {
                    let mut buf = [0u8; 64];
                    let n = libc::read(0, buf.as_mut_ptr().cast(), buf.len());
                    let retry = n < 0
                        && matches!(
                            io::Error::last_os_error().kind(),
                            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                        );
                    if n == 0 || (n < 0 && !retry) {
                        libc::kill(child, libc::SIGKILL);
                        break;
                    }
                }
            }
        }
        let mut status = 0;
        while libc::waitpid(child, &mut status, 0) < 0 {
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                libc::_exit(125);
            }
        }
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            128 + libc::WTERMSIG(status)
        };
        libc::_exit(code);
    }
}

/// Block until `alive` hangs up (the supervisor is gone), then exit at once.
pub fn exit_with_parent(alive: OwnedFd) -> ! {
    let mut pfd = libc::pollfd {
        fd: alive.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll on a descriptor we own; _exit skips destructors that
    // would race libkrun's running threads.
    unsafe {
        while libc::poll(&mut pfd, 1, -1) < 0
            && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
        {}
        libc::_exit(125)
    }
}

/// Drop root's supplementary groups; libkrun sets only the uid and gid.
pub fn drop_groups() -> io::Result<()> {
    // SAFETY: setgroups with an empty list.
    os(unsafe { libc::setgroups(0, std::ptr::null()) }).map(drop)
}

/// Bound what the VMM's uid can use until cgroups take over (M1 part 2):
/// processes and threads, open files, file size (the disk image is the only
/// file it writes, and it never grows), and no core dumps.
pub fn limit_resources(max_file: u64) -> io::Result<()> {
    let set = |res, v: u64| {
        let lim = libc::rlimit {
            rlim_cur: v,
            rlim_max: v,
        };
        // SAFETY: setrlimit with a valid struct.
        os(unsafe { libc::setrlimit(res, &lim) }).map(drop)
    };
    set(libc::RLIMIT_NPROC, 512)?;
    set(libc::RLIMIT_NOFILE, 1024)?;
    set(libc::RLIMIT_FSIZE, max_file)?;
    set(libc::RLIMIT_CORE, 0)
}

/// Syscalls a VMM never needs once the guest runs, and that an escape would
/// use to widen its reach: other processes, mounts, kernel modules and keys,
/// BPF, perf, userfaultfd, raw handles, io_uring. Namespaces and host vsock
/// are refused by rules of their own in [`vmm_filter`].
///
/// A deny list, not an allow list: libkrun's own syscall set is not pinned
/// down yet. An allow list replaces this once it is (DESIGN.md §4.2).
fn denied() -> Vec<libc::c_long> {
    let v = vec![
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_fsopen,
        libc::SYS_fsmount,
        libc::SYS_fspick,
        libc::SYS_move_mount,
        libc::SYS_open_tree,
        libc::SYS_mount_setattr,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_userfaultfd,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_acct,
        libc::SYS_quotactl,
        libc::SYS_syslog,
        libc::SYS_io_uring_setup,
    ];
    #[cfg(target_arch = "x86_64")]
    let v = [v, vec![libc::SYS_iopl, libc::SYS_ioperm, libc::SYS_uselib]].concat();
    v
}

/// The VMM filter: [`denied`], no namespaces (a user namespace would give
/// the VMM's uid capabilities over a new network namespace, and with them
/// netfilter and netlink), and no host vsock (vsock is not scoped by
/// network namespace; libkrun's own vsock device uses Unix sockets).
pub fn vmm_filter() -> Vec<libc::sock_filter> {
    let mut f = Filter::new();
    for nr in denied() {
        f = f.deny(nr, errno(libc::EPERM));
    }
    f.deny_namespaces()
        .deny_arg0_eq(
            libc::SYS_socket,
            libc::AF_VSOCK as u32,
            errno(libc::EAFNOSUPPORT),
        )
        .build()
}

/// Install the filter on every thread of the process; fails, installing
/// nothing, if any thread cannot take it. Threads libkrun starts later
/// inherit it.
pub fn install_filter(filter: &[libc::sock_filter]) -> io::Result<()> {
    sandpiper_seccomp::install(filter, true).map_err(|e| io::Error::other(format!("seccomp: {e}")))
}

/// Test the walls from inside, as the VMM will run: in the jail, as the
/// machine's uid, under the filter. Prints one line per wall and returns 0
/// if all held.
pub fn probe_walls(uid: u32, gid: u32) -> i32 {
    let ready = drop_groups()
        .and_then(|()| limit_resources(1 << 20))
        // SAFETY: setgid then setuid, as libkrun does at start.
        .and_then(|()| os(unsafe { libc::setgid(gid) }).map(drop))
        .and_then(|()| os(unsafe { libc::setuid(uid) }).map(drop))
        .and_then(|()| install_filter(&vmm_filter()));
    if let Err(e) = ready {
        eprintln!("probe setup failed: {e}");
        return 2;
    }
    // SAFETY: plain syscalls; anything that succeeds is undone or exits.
    let results = unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
        if fd >= 0 {
            libc::close(fd);
        }
        let clone = libc::syscall(
            libc::SYS_clone,
            (libc::CLONE_NEWUSER | libc::SIGCHLD) as libc::c_ulong,
            0usize,
            0usize,
            0usize,
            0usize,
        );
        if clone == 0 {
            libc::_exit(0);
        }
        if clone > 0 {
            libc::waitpid(clone as i32, std::ptr::null_mut(), 0);
        }
        [
            ("vsock", fd < 0),
            ("userns", libc::unshare(libc::CLONE_NEWUSER) != 0),
            ("clone-userns", clone < 0),
            ("ptrace", libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) != 0),
            (
                "mount",
                libc::mount(
                    c"none".as_ptr(),
                    c"/machine".as_ptr(),
                    c"tmpfs".as_ptr(),
                    0,
                    std::ptr::null(),
                ) != 0,
            ),
            ("host-files", fs::metadata("/etc/passwd").is_err()),
            ("root-readonly", fs::File::create("/probe").is_err()),
            ("uid", libc::geteuid() != 0 && libc::getegid() != 0),
        ]
    };
    let mut all = true;
    for (name, blocked) in results {
        println!("{name} {}", if blocked { "blocked" } else { "open" });
        all &= blocked;
    }
    if all { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandpiper_seccomp::{RET_ALLOW, simulate};

    #[test]
    fn vmm_filter_denies_what_it_lists_and_allows_the_rest() {
        let f = vmm_filter();
        let s = |nr, a| simulate(&f, None, nr, a);
        for nr in denied() {
            assert_eq!(s(nr, 0), errno(libc::EPERM));
        }
        assert_eq!(s(libc::SYS_unshare, 0), errno(libc::EPERM));
        assert_eq!(s(libc::SYS_clone3, 0), errno(libc::ENOSYS));
        assert_eq!(
            s(libc::SYS_clone, libc::CLONE_NEWUSER as u64),
            errno(libc::EPERM)
        );
        assert_eq!(
            s(libc::SYS_socket, libc::AF_VSOCK as u64),
            errno(libc::EAFNOSUPPORT)
        );
        for nr in [
            libc::SYS_read,
            libc::SYS_ioctl,
            libc::SYS_mmap,
            libc::SYS_setuid,
        ] {
            assert_eq!(s(nr, 0), RET_ALLOW);
        }
        assert_eq!(s(libc::SYS_socket, libc::AF_UNIX as u64), RET_ALLOW);
    }
}
