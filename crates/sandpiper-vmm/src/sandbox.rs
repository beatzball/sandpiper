//! The outer layer of the first wall (DESIGN.md §4.2), Linux only: the VMM
//! process gets its own mount, PID, network, IPC and UTS namespaces, sees
//! only its machine's files and a few device nodes, switches to its own
//! unprivileged uid just before the guest starts, and runs under a seccomp
//! filter that refuses the syscalls a VMM escape would reach for.
//!
//! The process must start as root. Order:
//!
//! 1. [`enter`]: new namespaces, fork (the child is PID 1 of the new PID
//!    namespace; the parent only waits and passes on the exit code), then
//!    a minimal root built on a tmpfs with `pivot_root`.
//! 2. libkrun is configured: it opens `/dev/kvm` and the disk as root.
//! 3. [`drop_groups`] and [`install_filter`] run in `before_enter`.
//! 4. `krun_start_enter` switches to the machine's uid and starts the guest.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

/// Where the machine's directory appears inside the sandbox.
pub const MACHINE_DIR: &str = "/machine";

/// World-usable device nodes, bound into the sandbox from the host.
const DEVICES: &[&str] = &["/dev/null", "/dev/zero", "/dev/urandom"];

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
/// The returned descriptor hangs up when the parent exits. The caller must
/// watch it and exit when it does: `PR_SET_PDEATHSIG` alone is not enough,
/// because the kernel clears it when libkrun switches to the machine's uid.
pub fn enter(scratch: &Path, machine: &Path, uid: u32, gid: u32) -> io::Result<OwnedFd> {
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
        // The parent holds the write end of this pipe until it exits. The
        // child cannot use getppid() to see whether the parent is still
        // there: across a PID namespace it always reads 0.
        os(libc::pipe2(alive.as_mut_ptr(), libc::O_CLOEXEC))?;
        let pid = os(libc::fork())?;
        if pid > 0 {
            // The parent stays outside the PID namespace and only waits. If it
            // is killed, the child gets SIGKILL from PR_SET_PDEATHSIG and the
            // whole namespace goes with it.
            libc::close(alive[0]);
            let mut status = 0;
            while libc::waitpid(pid, &mut status, 0) < 0 {
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
        libc::close(alive[1]);
        os(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0))?;
        // If the parent died before the signal was armed, the pipe has hung up.
        let mut pfd = libc::pollfd {
            fd: alive[0],
            events: libc::POLLIN,
            revents: 0,
        };
        if libc::poll(&mut pfd, 1, 0) != 0 {
            libc::_exit(125);
        }
    }
    // SAFETY: alive[0] is open and owned by nothing else from here on.
    let alive = unsafe { OwnedFd::from_raw_fd(alive[0]) };

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
    // wrapper, so it goes through syscall().
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

/// Block until `alive` hangs up (the parent is gone), then exit.
pub fn exit_with_parent(alive: OwnedFd) -> ! {
    let mut pfd = libc::pollfd {
        fd: alive.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll on a descriptor we own.
    while unsafe { libc::poll(&mut pfd, 1, -1) } < 0
        && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
    {}
    std::process::exit(125)
}

/// Drop root's supplementary groups; libkrun sets only the uid and gid.
pub fn drop_groups() -> io::Result<()> {
    // SAFETY: setgroups with an empty list.
    os(unsafe { libc::setgroups(0, std::ptr::null()) }).map(drop)
}

#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;

const LD_W_ABS: u16 = 0x20;
const JEQ_K: u16 = 0x15;
const JGE_K: u16 = 0x35;
const RET_K: u16 = 0x06;
const RET_ALLOW: u32 = 0x7fff_0000;
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;

/// Syscalls a VMM never needs once the guest runs, and that an escape would
/// use to widen its reach: other processes, mounts and namespaces, kernel
/// modules and keys, BPF, perf, userfaultfd, raw handles, io_uring.
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
        libc::SYS_unshare,
        libc::SYS_setns,
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

fn ins(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

pub fn vmm_filter() -> Vec<libc::sock_filter> {
    let mut f = vec![
        ins(LD_W_ABS, 4, 0, 0), // arch
        ins(JEQ_K, AUDIT_ARCH, 1, 0),
        ins(RET_K, RET_KILL_PROCESS, 0, 0),
        ins(LD_W_ABS, 0, 0, 0), // nr
    ];
    if cfg!(target_arch = "x86_64") {
        f.push(ins(JGE_K, 0x4000_0000, 0, 1));
        f.push(ins(RET_K, RET_KILL_PROCESS, 0, 0));
    }
    for nr in denied() {
        f.push(ins(JEQ_K, nr as u32, 0, 1));
        f.push(ins(RET_K, RET_ERRNO | libc::EPERM as u32, 0, 0));
    }
    f.push(ins(RET_K, RET_ALLOW, 0, 0));
    f
}

/// Install the filter for every thread of the process. Threads libkrun
/// starts later inherit it.
pub fn install_filter(filter: &[libc::sock_filter]) -> io::Result<()> {
    let prog = libc::sock_fprog {
        len: u16::try_from(filter.len()).map_err(|_| io::Error::other("filter too long"))?,
        filter: filter.as_ptr().cast_mut(),
    };
    // SAFETY: valid arguments; the kernel copies the program.
    unsafe {
        os(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
        os(libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_TSYNC,
            &prog as *const libc::sock_fprog,
        ) as libc::c_int)
        .map_err(|e| io::Error::other(format!("seccomp: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(f: &[libc::sock_filter], arch: u32, nr: u32) -> u32 {
        let (mut acc, mut pc) = (0u32, 0usize);
        loop {
            let i = f[pc];
            pc += 1;
            match i.code {
                LD_W_ABS => acc = if i.k == 4 { arch } else { nr },
                RET_K => return i.k,
                JEQ_K | JGE_K => {
                    let hit = if i.code == JEQ_K {
                        acc == i.k
                    } else {
                        acc >= i.k
                    };
                    pc += usize::from(if hit { i.jt } else { i.jf });
                }
                c => panic!("opcode {c:#x}"),
            }
        }
    }

    #[test]
    fn denies_the_listed_syscalls_and_allows_the_rest() {
        let f = vmm_filter();
        for nr in denied() {
            assert_eq!(
                eval(&f, AUDIT_ARCH, nr as u32),
                RET_ERRNO | libc::EPERM as u32
            );
        }
        for nr in [
            libc::SYS_read,
            libc::SYS_ioctl,
            libc::SYS_mmap,
            libc::SYS_setuid,
        ] {
            assert_eq!(eval(&f, AUDIT_ARCH, nr as u32), RET_ALLOW);
        }
        assert_eq!(eval(&f, 0x4000_0003, 0), RET_KILL_PROCESS);
    }
}
