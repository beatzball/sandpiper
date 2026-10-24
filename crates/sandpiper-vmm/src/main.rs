//! `sandpiper-vmm` — one microVM per process (DESIGN.md §4.2).
//!
//! ```text
//! sandpiper-vmm boot --root DIR --socket PATH [--cpus N] [--mem MIB] [--die-with-parent]
//! sandpiper-vmm run  --root DIR [--cpus N] [--mem MIB] [--timeout SECS] [--stdin] [--time] -- CMD [ARGS...]
//! ```
//!
//! `boot` enters the VM and never returns. `run` is the M0 driver: it starts
//! a `boot` child, connects to the guest agent, runs one command, prints its
//! output, and destroys the machine. The host agent replaces `run` in M1.
//!
//! `run` exits with the command's exit code (codes outside 0–255 become 255),
//! 124 if the command timed out, or 125 if sandpiper itself failed. A command
//! can also exit 124 or 125; the `sandpiper-vmm:` line on stderr tells them
//! apart.

mod krun;

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sandpiper_proto::{
    DEFAULT_OUTPUT_CAP, ExecRequest, Frame, GUEST_PORT, PROTOCOL_VERSION, read_frame, write_frame,
};

/// Where the guest agent lives inside every rootfs.
const GUEST_AGENT: &str = "/sandpiper/guest";
const GUEST_ENV: &[&str] = &[
    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "HOME=/root",
];
const BOOT_TIMEOUT: Duration = Duration::from_secs(10);
/// How much of the VMM's own console output is kept for error reports.
const CONSOLE_TAIL: usize = 64 * 1024;
/// Longest guest-supplied message printed to the terminal.
const MAX_GUEST_MESSAGE: usize = 4096;

const EXIT_TIMEOUT: u8 = 124;
const EXIT_SANDPIPER: u8 = 125;

struct Opts {
    root: Option<String>,
    socket: Option<String>,
    cpus: u8,
    mem: u32,
    timeout: Duration,
    stdin: bool,
    time: bool,
    die_with_parent: bool,
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
        die_with_parent: false,
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
            "--die-with-parent" => o.die_with_parent = true,
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
        ExitCode::from(EXIT_SANDPIPER)
    })
}

fn usage(err: &str) -> ExitCode {
    eprintln!("sandpiper-vmm: {err}");
    eprintln!(
        "usage: sandpiper-vmm boot --root DIR --socket PATH [--cpus N] [--mem MIB] [--die-with-parent]"
    );
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
    if o.die_with_parent {
        // The parent holds the write end of our stdin. However the parent
        // ends — even SIGKILL — the pipe closes and the machine goes with it.
        thread::spawn(|| {
            let mut sink = [0u8; 64];
            while matches!(io::stdin().read(&mut sink), Ok(n) if n > 0) {}
            std::process::exit(EXIT_SANDPIPER.into());
        });
    }
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

/// A private scratch directory for one machine's socket, removed on drop.
/// Kept short: macOS limits a Unix socket path to 104 bytes.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let dir = std::env::temp_dir().join(format!("sp-{}-{nanos:08x}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The last [`CONSOLE_TAIL`] bytes the VMM printed. Bounded in memory, and
/// nothing is written to disk, so a noisy guest console cannot fill either.
#[derive(Clone, Default)]
struct ConsoleTail(Arc<Mutex<VecDeque<u8>>>);

impl ConsoleTail {
    fn capture<R: Read + Send + 'static>(&self, mut src: R) {
        let tail = self.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n @ 1..) = src.read(&mut buf) {
                let mut t = tail.0.lock().expect("console lock");
                t.extend(&buf[..n]);
                let excess = t.len().saturating_sub(CONSOLE_TAIL);
                t.drain(..excess);
            }
        });
    }

    fn dump(&self) {
        let t = self.0.lock().expect("console lock");
        if t.is_empty() {
            return;
        }
        let bytes: Vec<u8> = t.iter().copied().collect();
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(40);
        eprintln!("--- VMM console (last {} lines) ---", lines.len() - start);
        for line in &lines[start..] {
            eprintln!("{}", sanitize(line, MAX_GUEST_MESSAGE));
        }
    }
}

/// Kills the VMM process when dropped, so no normal exit path leaks a
/// machine. Abnormal exits are covered by `--die-with-parent`.
struct Machine(Child);

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Kills the VMM when the current deadline passes, whatever the main thread
/// is blocked on: a trickling frame, a full socket, a silent guest. Killing
/// the VMM closes the socket, which unblocks every read and write.
struct Watchdog {
    state: Arc<Mutex<WatchState>>,
}

struct WatchState {
    deadline: Instant,
    done: bool,
    fired: bool,
}

impl Watchdog {
    fn start(pid: u32, deadline: Instant) -> Self {
        let state = Arc::new(Mutex::new(WatchState {
            deadline,
            done: false,
            fired: false,
        }));
        let s = Arc::clone(&state);
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_millis(20));
                let mut st = s.lock().expect("watchdog lock");
                if st.done {
                    return;
                }
                if Instant::now() >= st.deadline {
                    st.fired = true;
                    // SAFETY: plain kill(2). `done` is set, under this lock,
                    // before the child is reaped, so the pid is still ours.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                    return;
                }
            }
        });
        Self { state }
    }

    fn extend_to(&self, deadline: Instant) {
        self.state.lock().expect("watchdog lock").deadline = deadline;
    }

    fn fired(&self) -> bool {
        self.state.lock().expect("watchdog lock").fired
    }

    /// Must be called before the child is reaped.
    fn stop(&self) {
        self.state.lock().expect("watchdog lock").done = true;
    }
}

enum Outcome {
    Exited {
        code: i32,
        truncated: bool,
        ready_at: Duration,
    },
    TimedOut,
}

fn run(o: &Opts) -> io::Result<ExitCode> {
    if o.argv.is_empty() {
        return Err(io::Error::other("no command given after --"));
    }
    let root = fs::canonicalize(need(&o.root, "--root")?)?;
    let scratch = Scratch::new()?;
    let socket = scratch.0.join("agent.sock");

    let started = Instant::now();
    let (lifeline_r, lifeline_w) = io::pipe()?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("boot")
        .arg("--root")
        .arg(&root)
        .arg("--socket")
        .arg(&socket)
        .args(["--cpus", &o.cpus.to_string(), "--mem", &o.mem.to_string()])
        .arg("--die-with-parent")
        .stdin(lifeline_r)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let console = ConsoleTail::default();
    console.capture(child.stdout.take().expect("piped"));
    console.capture(child.stderr.take().expect("piped"));
    let watchdog = Watchdog::start(child.id(), started + BOOT_TIMEOUT);
    let mut machine = Machine(child);

    let outcome = exchange(o, &socket, &mut machine, &watchdog, started);
    watchdog.stop();
    let outcome = match outcome {
        Ok(outcome) => outcome,
        // A failure caused by the watchdog's kill is a timeout, not an error.
        Err(_) if watchdog.fired() => Outcome::TimedOut,
        Err(e) => {
            drop(machine);
            console.dump();
            return Err(e);
        }
    };
    drop(machine);
    drop(lifeline_w);

    match outcome {
        Outcome::TimedOut => {
            eprintln!("sandpiper-vmm: timed out; machine destroyed");
            console.dump();
            Ok(ExitCode::from(EXIT_TIMEOUT))
        }
        Outcome::Exited {
            code,
            truncated,
            ready_at,
        } => {
            if o.time {
                eprintln!(
                    "sandpiper: agent ready in {} ms, command done in {} ms",
                    ready_at.as_millis(),
                    started.elapsed().as_millis()
                );
            }
            if truncated {
                eprintln!("sandpiper: output was truncated at the per-stream cap");
            }
            Ok(ExitCode::from(u8::try_from(code).unwrap_or(255)))
        }
    }
}

fn exchange(
    o: &Opts,
    socket: &Path,
    machine: &mut Machine,
    watchdog: &Watchdog,
    started: Instant,
) -> io::Result<Outcome> {
    let mut conn = match connect(socket, machine, watchdog)? {
        Some(conn) => conn,
        None => return Ok(Outcome::TimedOut),
    };
    let ready_at = started.elapsed();
    watchdog.extend_to(Instant::now() + o.timeout);
    conn.set_read_timeout(None)?;

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
    // machine is destroyed when `run` returns, which ends any write in flight.
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

    // The guest promises to cap each stream; the host does not rely on it.
    let mut out = CappedSink::new(io::stdout().lock());
    let mut err = CappedSink::new(io::stderr().lock());
    loop {
        match read_frame(&mut conn)? {
            Some(Frame::Stdout(b)) => out.write(&b)?,
            Some(Frame::Stderr(b)) => err.write(&b)?,
            Some(Frame::Error(msg)) => {
                eprintln!("sandpiper: {}", sanitize(&msg, MAX_GUEST_MESSAGE))
            }
            Some(Frame::Exit(status)) => {
                out.inner.flush()?;
                let truncated = status.stdout_truncated
                    || status.stderr_truncated
                    || out.truncated
                    || err.truncated;
                return Ok(Outcome::Exited {
                    code: status.code,
                    truncated,
                    ready_at,
                });
            }
            Some(other) => {
                return Err(io::Error::other(format!(
                    "unexpected {} frame from guest",
                    other.kind()
                )));
            }
            None => return Err(io::Error::other("guest closed the connection before Exit")),
        }
    }
}

/// Writes at most [`DEFAULT_OUTPUT_CAP`] bytes and records whether more came.
struct CappedSink<W: Write> {
    inner: W,
    written: usize,
    truncated: bool,
}

impl<W: Write> CappedSink<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            written: 0,
            truncated: false,
        }
    }

    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        let room = DEFAULT_OUTPUT_CAP.saturating_sub(self.written);
        let take = data.len().min(room);
        if take < data.len() {
            self.truncated = true;
        }
        self.inner.write_all(&data[..take])?;
        self.written += take;
        Ok(())
    }
}

/// Connect to the guest agent, retrying until it answers Ready. libkrun owns
/// the listening socket, so a connect can succeed before the guest listens;
/// only a Ready frame proves the agent is there. Returns `None` if the
/// watchdog fired during boot.
fn connect(
    socket: &Path,
    machine: &mut Machine,
    watchdog: &Watchdog,
) -> io::Result<Option<UnixStream>> {
    let retryable = |e: &io::Error| {
        matches!(
            e.kind(),
            io::ErrorKind::NotFound
                | io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::WouldBlock
                | io::ErrorKind::TimedOut
        )
    };
    loop {
        if watchdog.fired() {
            return Ok(None);
        }
        if let Some(status) = machine.0.try_wait()? {
            if watchdog.fired() {
                return Ok(None);
            }
            return Err(io::Error::other(format!(
                "VMM exited during boot ({status})"
            )));
        }
        match UnixStream::connect(socket) {
            Ok(mut conn) => {
                conn.set_read_timeout(Some(Duration::from_millis(500)))?;
                match read_frame(&mut conn) {
                    Ok(Some(Frame::Ready { version })) if version == PROTOCOL_VERSION => {
                        return Ok(Some(conn));
                    }
                    Ok(Some(Frame::Ready { version })) => {
                        return Err(io::Error::other(format!(
                            "guest speaks protocol {version}, host {PROTOCOL_VERSION}"
                        )));
                    }
                    Ok(Some(other)) => {
                        return Err(io::Error::other(format!(
                            "expected Ready, got {} frame",
                            other.kind()
                        )));
                    }
                    Ok(None) => {}
                    Err(e) if retryable(&e) => {}
                    Err(e) => return Err(e),
                }
            }
            Err(e) if retryable(&e) => {}
            Err(e) => return Err(e),
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Cut guest-supplied text to `max` bytes and escape control characters, so
/// a guest cannot flood the terminal or send it escape sequences.
fn sanitize(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.len() >= max {
            out.push('…');
            break;
        }
        if c.is_control() && c != '\t' {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_escapes_and_cuts() {
        assert_eq!(sanitize("ok\tfine", 100), "ok\tfine");
        assert_eq!(sanitize("\u{1b}[2J", 100), "\\u{1b}[2J");
        assert_eq!(sanitize("abcdef", 3), "abc…");
    }

    #[test]
    fn capped_sink_stops_at_cap_and_records_it() {
        let mut sink = CappedSink::new(Vec::new());
        sink.write(&vec![1; DEFAULT_OUTPUT_CAP - 1]).unwrap();
        assert!(!sink.truncated);
        sink.write(&[2, 3]).unwrap();
        assert!(sink.truncated);
        assert_eq!(sink.inner.len(), DEFAULT_OUTPUT_CAP);
        sink.write(&[4]).unwrap();
        assert_eq!(sink.inner.len(), DEFAULT_OUTPUT_CAP);
    }

    #[test]
    fn console_tail_is_bounded() {
        let tail = ConsoleTail::default();
        {
            let mut t = tail.0.lock().unwrap();
            t.extend(std::iter::repeat_n(b'x', CONSOLE_TAIL));
        }
        tail.capture(&b"tail-end"[..]);
        for _ in 0..100 {
            if tail
                .0
                .lock()
                .unwrap()
                .iter()
                .rev()
                .take(8)
                .eq(b"tail-end".iter().rev())
            {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let t = tail.0.lock().unwrap();
        assert_eq!(t.len(), CONSOLE_TAIL);
        assert!(t.iter().rev().take(8).eq(b"tail-end".iter().rev()));
    }
}
