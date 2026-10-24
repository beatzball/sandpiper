//! The guest agent (DESIGN.md §4.4).
//!
//! Binds the vsock port at boot, before any workload exists, then serves one
//! exchange per connection. The host always connects in; this process never
//! dials out.
//!
//! Not yet here (M2): dropping workloads to an unprivileged uid and the
//! seccomp filter that keeps them off vsock.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    if let Err(e) = linux::serve() {
        eprintln!("sandpiper-guest: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sandpiper-guest runs only inside a Linux guest");
    std::process::exit(1);
}
