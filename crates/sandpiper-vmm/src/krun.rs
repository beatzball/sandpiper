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
    }
}

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

pub struct VmConfig<'a> {
    pub root: &'a str,
    pub vcpus: u8,
    pub ram_mib: u32,
    /// Host Unix socket that libkrun listens on and forwards to the guest port.
    pub socket: &'a str,
    pub guest_port: u32,
    pub exec_path: &'a str,
    /// Passed explicitly: a NULL envp would copy the host environment
    /// (home directory, user name) into the guest.
    pub env: &'a [&'a str],
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
        check(
            "krun_set_root",
            ffi::krun_set_root(ctx, cstr(cfg.root)?.as_ptr()),
        )?;
        // No TSI: the guest gets vsock for the agent and no network at all.
        check(
            "krun_disable_implicit_vsock",
            ffi::krun_disable_implicit_vsock(ctx),
        )?;
        check("krun_add_vsock", ffi::krun_add_vsock(ctx, 0))?;
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
        let rc = ffi::krun_start_enter(ctx);
        Err(io::Error::other(format!("krun_start_enter returned {rc}")))
    }
}
