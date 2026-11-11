//! The guest agent (DESIGN.md §4.4).
//!
//! Binds the vsock port at boot, before any workload exists, then serves one
//! exchange per connection. The host always connects in; this process never
//! dials out. Workloads run unprivileged and cannot reach vsock (`confine`).

#[cfg(target_os = "linux")]
mod confine;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    // `probe` runs as a workload (through exec) to test the walls from the
    // inside; see `confine::probe` and `confine::probe_walls`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [mode, what, rest @ ..] = args.as_slice()
        && mode == "probe"
    {
        std::process::exit(match what.as_str() {
            "walls" => match rest {
                [code, canaries @ ..] => confine::probe_walls(code, canaries),
                [] => 2,
            },
            _ => confine::probe(what),
        });
    }
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
