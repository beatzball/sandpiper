//! `sandpiperd` — the host agent (DESIGN.md §4.2). Runs machines on this
//! host and serves the API (§6).
//!
//! ```text
//! sandpiperd [--listen ADDR] [--state DIR] [--vmm PATH] [--max-machines N]
//!            [--cpus N] [--mem MIB]
//! ```
//!
//! Templates are disk images at `<state>/templates/<name>.img`. The API
//! token is read from `<state>/token`, made on first start. On Linux the
//! host agent runs as root and sandboxes every VMM; setting
//! `SANDPIPER_UNSANDBOXED=1` skips the sandbox, for development only.

mod api;
#[cfg(target_os = "linux")]
mod cgroup;
mod events;
mod ids;
mod machines;

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use machines::{Config, Manager};

struct Opts {
    listen: String,
    state: PathBuf,
    vmm: Option<PathBuf>,
    max_machines: usize,
    cpus: u8,
    mem: u32,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        listen: "127.0.0.1:7171".into(),
        state: PathBuf::from("/var/lib/sandpiper"),
        vmm: None,
        max_machines: 16,
        cpus: 1,
        mem: 512,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--listen" => o.listen = val("--listen")?,
            "--state" => o.state = val("--state")?.into(),
            "--vmm" => o.vmm = Some(val("--vmm")?.into()),
            "--max-machines" => {
                o.max_machines = val("--max-machines")?
                    .parse()
                    .map_err(|_| "--max-machines must be a number")?
            }
            "--cpus" => {
                o.cpus = val("--cpus")?
                    .parse()
                    .map_err(|_| "--cpus must be a number")?
            }
            "--mem" => {
                o.mem = val("--mem")?
                    .parse()
                    .map_err(|_| "--mem must be a number")?
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(o)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("sandpiperd: {e}");
            eprintln!(
                "usage: sandpiperd [--listen ADDR] [--state DIR] [--vmm PATH] [--max-machines N] [--cpus N] [--mem MIB]"
            );
            return ExitCode::from(2);
        }
    };
    match run(opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sandpiperd: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(o: Opts) -> io::Result<()> {
    // SAFETY: geteuid has no preconditions.
    let root = unsafe { libc::geteuid() } == 0;
    let unsandboxed = std::env::var_os("SANDPIPER_UNSANDBOXED").is_some();
    let sandbox = cfg!(target_os = "linux") && !unsandboxed;
    if sandbox && !root {
        return Err(io::Error::other(
            "the host agent sandboxes machines, which needs root; set SANDPIPER_UNSANDBOXED=1 for development",
        ));
    }
    if !sandbox {
        eprintln!("sandpiperd: machines are NOT sandboxed (development only)");
    }
    let vmm = match o.vmm {
        Some(p) => p,
        None => std::env::current_exe()?.with_file_name("sandpiper-vmm"),
    };
    fs::DirBuilder::new().recursive(true).create(&o.state)?;
    let state = fs::canonicalize(&o.state)?;
    let token = load_token(&state.join("token"))?;
    let log = Arc::new(events::Log::open(&state.join("events.db"))?);
    let mgr = Manager::start(
        Config {
            state: state.clone(),
            vmm,
            sandbox,
            max_machines: o.max_machines,
            cpus: o.cpus,
            mem_mib: o.mem,
            idle: Duration::from_secs(15 * 60),
            max: Duration::from_secs(4 * 3600),
            exec_max: Duration::from_secs(3600),
        },
        Arc::clone(&log),
    )?;
    {
        let log = Arc::clone(&log);
        std::thread::spawn(move || {
            loop {
                if let Err(e) = log.purge() {
                    eprintln!("sandpiperd: {e}");
                }
                std::thread::sleep(Duration::from_secs(3600));
            }
        });
    }
    let app = Arc::new(api::App {
        mgr: Arc::clone(&mgr),
        log,
        token,
    });
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(&o.listen).await?;
        eprintln!(
            "sandpiperd: listening on {}; token in {}",
            listener.local_addr()?,
            state.join("token").display()
        );
        axum::serve(listener, api::router(app))
            .with_graceful_shutdown(shutdown_signal())
            .await
    })?;
    eprintln!("sandpiperd: stopping; destroying every machine");
    mgr.destroy_all("host agent stopping");
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("signal handler");
    let mut int = signal(SignalKind::interrupt()).expect("signal handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// Read the API token, or make one: 32 random bytes as hex, readable by
/// the owner only.
fn load_token(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(t) if t.trim().len() >= 32 => return Ok(t.trim().to_owned()),
        Ok(_) => {
            return Err(io::Error::other(format!(
                "{} holds a token shorter than 32 characters",
                path.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let token = ids::random_hex(32);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(format!("{token}\n").as_bytes())?;
    Ok(token)
}

/// Cut guest-supplied text to `max` bytes and escape control characters,
/// so it cannot send a terminal escape sequences.
pub fn sanitize(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.len() >= max {
            out.push('…');
            break;
        }
        if c.is_control() && c != '\t' && c != '\n' {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}
