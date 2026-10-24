//! `sandpiper-vmm` — one microVM per process (DESIGN.md §4.2).
//!
//! ```text
//! sandpiper-vmm boot --root DIR --socket PATH [--cpus N] [--mem MIB]
//! sandpiper-vmm run  --root DIR [--cpus N] [--mem MIB] [--timeout SECS] [--stdin] [--time] -- CMD [ARGS...]
//! ```
//!
//! `boot` enters the VM and never returns. `run` is the M0 driver: it starts
//! a `boot` child, connects to the guest agent, runs one command, prints its
//! output, and destroys the machine. The host agent replaces `run` in M1.

mod krun;

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sandpiper_proto::{
    ExecRequest, ExitStatus, Frame, GUEST_PORT, PROTOCOL_VERSION, read_frame, write_frame,
};

/// Where the guest agent lives inside every rootfs.
const GUEST_AGENT: &str = "/sandpiper/guest";
const GUEST_ENV: &[&str] = &[
    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "HOME=/root",
];
const BOOT_TIMEOUT: Duration = Duration::from_secs(10);

struct Opts {
    root: Option<String>,
    socket: Option<String>,
    cpus: u8,
    mem: u32,
    timeout: Duration,
    stdin: bool,
    time: bool,
    argv: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        root: None,
        socket: None,
        cpus: 1,
        mem: 512,
        timeout: Duration::from_secs(30),
        stdin: false,
        time: false,
        argv: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--root" => o.root = Some(val("--root")?),
            "--socket" => o.socket = Some(val("--socket")?),
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
            "--timeout" => {
                o.timeout = Duration::from_secs(
                    val("--timeout")?
                        .parse()
                        .map_err(|_| "--timeout must be seconds")?,
                )
            }
            "--stdin" => o.stdin = true,
            "--time" => o.time = true,
            "--" => {
                o.argv = it.cloned().collect();
                break;
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(o)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => ("", &[][..]),
    };
    let opts = match parse(rest) {
        Ok(o) => o,
        Err(e) => return usage(&e),
    };
    let result = match cmd {
        "boot" => boot(&opts).map(|_| ExitCode::SUCCESS),
        "run" => run(&opts),
        _ => return usage("expected `boot` or `run`"),
    };
    result.unwrap_or_else(|e| {
        eprintln!("sandpiper-vmm: {e}");
        ExitCode::from(125)
    })
}

fn usage(err: &str) -> ExitCode {
    eprintln!("sandpiper-vmm: {err}");
    eprintln!("usage: sandpiper-vmm boot --root DIR --socket PATH [--cpus N] [--mem MIB]");
    eprintln!(
        "       sandpiper-vmm run --root DIR [--timeout SECS] [--stdin] [--time] -- CMD [ARGS...]"
    );
    ExitCode::from(2)
}

fn need<'a>(v: &'a Option<String>, name: &str) -> io::Result<&'a str> {
    v.as_deref()
        .ok_or_else(|| io::Error::other(format!("{name} is required")))
}

fn boot(o: &Opts) -> io::Result<std::convert::Infallible> {
    krun::boot(&krun::VmConfig {
        root: need(&o.root, "--root")?,
        vcpus: o.cpus,
        ram_mib: o.mem,
        socket: need(&o.socket, "--socket")?,
        guest_port: GUEST_PORT,
        exec_path: GUEST_AGENT,
        env: GUEST_ENV,
    })
}

/// A private scratch directory for one machine's socket and log, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("sandpiper-{}-{nanos}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Kills the VMM process when dropped, so no exit path leaks a machine.
struct Machine(Child);

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(o: &Opts) -> io::Result<ExitCode> {
    if o.argv.is_empty() {
        return Err(io::Error::other("no command given after --"));
    }
    let root = fs::canonicalize(need(&o.root, "--root")?)?;
    let scratch = Scratch::new()?;
    let socket = scratch.0.join("agent.sock");
    let log_path = scratch.0.join("vmm.log");

    let started = Instant::now();
    let log = fs::File::create(&log_path)?;
    let child = Command::new(std::env::current_exe()?)
        .arg("boot")
        .arg("--root")
        .arg(&root)
        .arg("--socket")
        .arg(&socket)
        .args(["--cpus", &o.cpus.to_string(), "--mem", &o.mem.to_string()])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;
    let mut machine = Machine(child);

    let (status, ready_at) = match exchange(o, &socket, &mut machine, started) {
        Ok(ok) => ok,
        Err(e) if e.kind() == io::ErrorKind::TimedOut => {
            eprintln!("sandpiper-vmm: {e}");
            return Ok(ExitCode::from(124));
        }
        Err(e) => {
            dump_log(&log_path);
            return Err(e);
        }
    };
    if o.time {
        eprintln!(
            "sandpiper: agent ready in {} ms, command done in {} ms",
            ready_at.as_millis(),
            started.elapsed().as_millis()
        );
    }
    if status.stdout_truncated || status.stderr_truncated {
        eprintln!("sandpiper: output was truncated at the per-stream cap");
    }
    Ok(ExitCode::from(status.code.clamp(0, 255) as u8))
}

fn exchange(
    o: &Opts,
    socket: &Path,
    machine: &mut Machine,
    started: Instant,
) -> io::Result<(ExitStatus, Duration)> {
    let mut conn = connect(socket, machine, started)?;
    let ready_at = started.elapsed();

    let deadline = Instant::now() + o.timeout;
    write_frame(
        &mut conn,
        &Frame::Exec(ExecRequest {
            argv: o.argv.clone(),
            env: Vec::new(),
            cwd: None,
        }),
    )?;
    // Feed stdin from its own thread so output is read while input is still
    // being sent; otherwise a command that echoes a large input fills both
    // socket buffers and neither side moves. The thread is never joined: the
    // machine is destroyed when this function's caller returns, which ends
    // any write still in flight.
    let mut feeder = conn.try_clone()?;
    let feed_stdin = o.stdin;
    thread::spawn(move || -> io::Result<()> {
        if feed_stdin {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                let n = io::stdin().read(&mut buf)?;
                if n == 0 {
                    break;
                }
                write_frame(&mut feeder, &Frame::Stdin(buf[..n].to_vec()))?;
            }
        }
        write_frame(&mut feeder, &Frame::Stdin(Vec::new()))
    });

    let mut out = io::stdout().lock();
    let mut err = io::stderr().lock();
    let timed_out = || {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "command timed out; machine destroyed",
        )
    };
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(timed_out());
        }
        conn.set_read_timeout(Some(left))?;
        let frame = match read_frame(&mut conn) {
            Ok(frame) => frame,
            // A socket read timeout surfaces as WouldBlock on macOS and Linux.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(timed_out());
            }
            Err(e) => return Err(e),
        };
        match frame {
            Some(Frame::Stdout(b)) => out.write_all(&b)?,
            Some(Frame::Stderr(b)) => err.write_all(&b)?,
            Some(Frame::Error(msg)) => writeln!(err, "sandpiper: {msg}")?,
            Some(Frame::Exit(status)) => {
                out.flush()?;
                return Ok((status, ready_at));
            }
            Some(other) => {
                return Err(io::Error::other(format!(
                    "unexpected frame from guest: {other:?}"
                )));
            }
            None => return Err(io::Error::other("guest closed the connection before Exit")),
        }
    }
}

/// Connect to the guest agent, retrying until it answers Ready. libkrun owns
/// the listening socket, so a connect can succeed before the guest listens;
/// only a Ready frame proves the agent is there.
fn connect(socket: &Path, machine: &mut Machine, started: Instant) -> io::Result<UnixStream> {
    loop {
        if let Some(status) = machine.0.try_wait()? {
            return Err(io::Error::other(format!(
                "VMM exited during boot ({status})"
            )));
        }
        if started.elapsed() > BOOT_TIMEOUT {
            return Err(io::Error::other("guest agent did not become ready in time"));
        }
        if let Ok(mut conn) = UnixStream::connect(socket) {
            conn.set_read_timeout(Some(Duration::from_millis(500)))?;
            match read_frame(&mut conn) {
                Ok(Some(Frame::Ready { version })) if version == PROTOCOL_VERSION => {
                    return Ok(conn);
                }
                Ok(Some(Frame::Ready { version })) => {
                    return Err(io::Error::other(format!(
                        "guest speaks protocol {version}, host {PROTOCOL_VERSION}"
                    )));
                }
                Ok(Some(other)) => {
                    return Err(io::Error::other(format!("expected Ready, got {other:?}")));
                }
                Ok(None) | Err(_) => {}
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn dump_log(path: &Path) {
    if let Ok(log) = fs::read_to_string(path) {
        let tail: Vec<&str> = log.lines().rev().take(40).collect();
        if !tail.is_empty() {
            eprintln!("--- VMM log (last {} lines) ---", tail.len());
            for line in tail.iter().rev() {
                eprintln!("{line}");
            }
        }
    }
}
