//! The slice of libkrun's C API that sandpiper uses (tested against 1.19.7).

use std::ffi::{CString, c_char};
use std::io;

mod ffi {
    use std::ffi::c_char;

    unsafe extern "C" {
        pub fn krun_set_log_level(level: u32) -> i32;
        pub fn krun_create_ctx() -> i32;
        pub fn krun_set_vm_config(ctx: u32, num_vcpus: u8, ram_mib: u32) -> i32;
        pub fn krun_set_root(ctx: u32, root_path: *const c_char) -> i32;
        pub fn krun_add_disk(
            ctx: u32,
            block_id: *const c_char,
            disk_path: *const c_char,
            read_only: bool,
        ) -> i32;
        pub fn krun_set_root_disk_remount(
            ctx: u32,
            device: *const c_char,
            fstype: *const c_char,
            options: *const c_char,
        ) -> i32;
        pub fn krun_setuid(ctx: u32, uid: u32) -> i32;
        pub fn krun_setgid(ctx: u32, gid: u32) -> i32;
        pub fn krun_disable_implicit_vsock(ctx: u32) -> i32;
        pub fn krun_add_vsock(ctx: u32, tsi_features: u32) -> i32;
        pub fn krun_add_vsock_port2(ctx: u32, port: u32, path: *const c_char, listen: bool) -> i32;
        pub fn krun_set_workdir(ctx: u32, path: *const c_char) -> i32;
        pub fn krun_set_exec(
            ctx: u32,
            exec_path: *const c_char,
            argv: *const *const c_char,
            envp: *const *const c_char,
        ) -> i32;
        pub fn krun_start_enter(ctx: u32) -> i32;
        #[cfg(feature = "wall-tests")]
        pub fn krun_add_net_unixstream(
            ctx: u32,
            path: *const c_char,
            fd: i32,
            mac: *const u8,
            features: u32,
            flags: u32,
        ) -> i32;
    }
}

/// A wall left open on purpose, in test builds, so the host agent's wall
/// check can be shown to catch it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Breach {
    #[default]
    None,
    /// TSI on: the guest's sockets act as the VMM process.
    #[cfg(feature = "wall-tests")]
    Tsi,
    /// A network device, backed by this end of a socket pair.
    #[cfg(feature = "wall-tests")]
    NetDevice(i32),
}

#[cfg(feature = "wall-tests")]
const KRUN_TSI_HIJACK_INET: u32 = 1;

fn check(what: &str, rc: i32) -> io::Result<i32> {
    if rc < 0 {
        Err(io::Error::other(format!(
            "{what} failed: {}",
            io::Error::from_raw_os_error(-rc)
        )))
    } else {
        Ok(rc)
    }
}

fn cstr(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::other(format!("NUL byte in {s:?}")))
}

/// A NULL-terminated array of C strings that owns its storage.
struct CStrArray {
    _owned: Vec<CString>,
    ptrs: Vec<*const c_char>,
}

impl CStrArray {
    fn new(items: &[&str]) -> io::Result<Self> {
        let owned = items
            .iter()
            .map(|s| cstr(s))
            .collect::<io::Result<Vec<_>>>()?;
        let mut ptrs: Vec<_> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        Ok(Self {
            _owned: owned,
            ptrs,
        })
    }
}

pub enum Root<'a> {
    /// A host directory shared over virtio-fs (M0, development only).
    Dir(&'a str),
    /// An ext4 image attached as the first virtio-blk device and mounted as
    /// `/`. The image is this machine's own copy: the guest writes to it.
    Disk(&'a str),
}

pub struct VmConfig<'a> {
    pub root: Root<'a>,
    pub vcpus: u8,
    pub ram_mib: u32,
    /// Host Unix socket that libkrun listens on and forwards to the guest port.
    pub socket: &'a str,
    pub guest_port: u32,
    pub exec_path: &'a str,
    /// Passed explicitly: a NULL envp would copy the host environment
    /// (home directory, user name) into the guest.
    pub env: &'a [&'a str],
    /// The unprivileged uid and gid libkrun switches to inside
    /// `krun_start_enter`, before it opens `/dev/kvm` and the disk: both
    /// must be usable by this uid.
    pub run_as: Option<(u32, u32)>,
    pub breach: Breach,
    /// Runs last, after libkrun is configured and before it takes over the
    /// process. The sandbox installs its seccomp filter here.
    pub before_enter: &'a dyn Fn() -> io::Result<()>,
}

/// Configure a microVM and enter it. On success this never returns: libkrun
/// takes over the process and exits it with the guest's exit code.
pub fn boot(cfg: &VmConfig) -> io::Result<std::convert::Infallible> {
    // SAFETY: each call gets a valid context id and NUL-terminated strings
    // that outlive the call; return codes are checked.
    unsafe {
        if let Ok(level) = std::env::var("SANDPIPER_KRUN_LOG") {
            ffi::krun_set_log_level(level.parse().unwrap_or(0));
        }
        let ctx = check("krun_create_ctx", ffi::krun_create_ctx())? as u32;
        check(
            "krun_set_vm_config",
            ffi::krun_set_vm_config(ctx, cfg.vcpus, cfg.ram_mib),
        )?;
        match cfg.root {
            Root::Dir(dir) => {
                check(
                    "krun_set_root",
                    ffi::krun_set_root(ctx, cstr(dir)?.as_ptr()),
                )?;
            }
            Root::Disk(image) => {
                check(
                    "krun_add_disk",
                    ffi::krun_add_disk(ctx, cstr("root")?.as_ptr(), cstr(image)?.as_ptr(), false),
                )?;
                check(
                    "krun_set_root_disk_remount",
                    ffi::krun_set_root_disk_remount(
                        ctx,
                        cstr("/dev/vda")?.as_ptr(),
                        cstr("ext4")?.as_ptr(),
                        std::ptr::null(),
                    ),
                )?;
            }
        }
        // No TSI: the guest gets vsock for the agent and no network at all.
        check(
            "krun_disable_implicit_vsock",
            ffi::krun_disable_implicit_vsock(ctx),
        )?;
        let tsi = match cfg.breach {
            #[cfg(feature = "wall-tests")]
            Breach::Tsi => KRUN_TSI_HIJACK_INET,
            _ => 0,
        };
        check("krun_add_vsock", ffi::krun_add_vsock(ctx, tsi))?;
        #[cfg(feature = "wall-tests")]
        if let Breach::NetDevice(fd) = cfg.breach {
            let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
            check(
                "krun_add_net_unixstream",
                ffi::krun_add_net_unixstream(ctx, std::ptr::null(), fd, mac.as_ptr(), 0, 0),
            )?;
        }
        check(
            "krun_add_vsock_port2",
            ffi::krun_add_vsock_port2(ctx, cfg.guest_port, cstr(cfg.socket)?.as_ptr(), true),
        )?;
        check(
            "krun_set_workdir",
            ffi::krun_set_workdir(ctx, cstr("/")?.as_ptr()),
        )?;
        let argv = CStrArray::new(&[])?;
        let envp = CStrArray::new(cfg.env)?;
        check(
            "krun_set_exec",
            ffi::krun_set_exec(
                ctx,
                cstr(cfg.exec_path)?.as_ptr(),
                argv.ptrs.as_ptr(),
                envp.ptrs.as_ptr(),
            ),
        )?;
        if let Some((uid, gid)) = cfg.run_as {
            check("krun_setgid", ffi::krun_setgid(ctx, gid))?;
            check("krun_setuid", ffi::krun_setuid(ctx, uid))?;
        }
        (cfg.before_enter)()?;
        let rc = ffi::krun_start_enter(ctx);
        Err(io::Error::other(format!("krun_start_enter returned {rc}")))
    }
}
