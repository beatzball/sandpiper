//! `sandpiper-vmm` — one microVM per process (DESIGN.md §4.2).
//!
//! ```text
//! sandpiper-vmm boot (--root DIR | --disk IMAGE) --socket PATH [--cpus N] [--mem MIB]
//!                    [--die-with-parent] [--jail DIR --uid N --gid N]
//! sandpiper-vmm run  (--root DIR | --disk IMAGE) [--cpus N] [--mem MIB] [--timeout SECS]
//!                    [--stdin] [--time] -- CMD [ARGS...]
//! ```
//!
//! `--disk` boots from an ext4 image; `run` gives each machine its own copy.
//! On Linux, `run` as root sandboxes the VMM (see `sandbox`): its own
//! namespaces and uid, only its machine's files, a seccomp filter. Without
//! root it warns and runs unsandboxed, for development; setting
//! `SANDPIPER_UNSANDBOXED=1` acknowledges that and silences the warning.
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
#[cfg(target_os = "linux")]
mod sandbox;

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
    disk: Option<String>,
    jail: Option<String>,
    uid: Option<u32>,
    gid: Option<u32>,
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
        disk: None,
        jail: None,
        uid: None,
        gid: None,
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
            "--disk" => o.disk = Some(val("--disk")?),
            "--jail" => o.jail = Some(val("--jail")?),
            "--uid" => {
                o.uid = Some(
                    val("--uid")?
                        .parse()
                        .map_err(|_| "--uid must be a number")?,
                )
            }
            "--gid" => {
                o.gid = Some(
                    val("--gid")?
                        .parse()
                        .map_err(|_| "--gid must be a number")?,
                )
            }
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
        "usage: sandpiper-vmm boot (--root DIR | --disk IMAGE) --socket PATH [--cpus N] [--mem MIB]"
    );
    eprintln!("                          [--die-with-parent] [--jail DIR --uid N --gid N]");
    eprintln!(
        "       sandpiper-vmm run (--root DIR | --disk IMAGE) [--timeout SECS] [--stdin] [--time] -- CMD [ARGS...]"
    );
    ExitCode::from(2)
}

fn need<'a>(v: &'a Option<String>, name: &str) -> io::Result<&'a str> {
    v.as_deref()
        .ok_or_else(|| io::Error::other(format!("{name} is required")))
}

fn boot(o: &Opts) -> io::Result<std::convert::Infallible> {
    let mut socket = need(&o.socket, "--socket")?.to_owned();
    let mut disk = o.disk.clone();
    let run_as = match (&o.jail, o.uid, o.gid) {
        (None, None, None) => None,
        (Some(jail), Some(uid), Some(gid)) => {
            enter_jail(Path::new(jail), &mut socket, &mut disk, uid, gid)?;
            Some((uid, gid))
        }
        _ => return Err(io::Error::other("--jail, --uid and --gid go together")),
    };
    // Threads start only after the jail is entered: it forks.
    if o.die_with_parent {
        // The parent holds the write end of our stdin. However the parent
        // ends — even SIGKILL — the pipe closes and the machine goes with it.
        thread::spawn(|| {
            let mut sink = [0u8; 64];
            loop {
                match io::stdin().read(&mut sink) {
                    Ok(0) => break,
                    Ok(_) => {}
                    // Tolerate the fd being made non-blocking under us.
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
            std::process::exit(EXIT_SANDPIPER.into());
        });
    }
    let root = match (&o.root, &disk) {
        (Some(dir), None) => krun::Root::Dir(dir),
        (None, Some(image)) => krun::Root::Disk(image),
        _ => return Err(io::Error::other("give exactly one of --root and --disk")),
    };
    krun::boot(&krun::VmConfig {
        root,
        vcpus: o.cpus,
        ram_mib: o.mem,
        socket: &socket,
        guest_port: GUEST_PORT,
        exec_path: GUEST_AGENT,
        env: GUEST_ENV,
        run_as,
        before_enter: &|| before_enter(run_as.is_some()),
    })
}

/// Enter the sandbox and rewrite the machine's paths to where they appear
/// inside it. The socket and disk must both live in `<jail>/machine`.
#[cfg(target_os = "linux")]
fn enter_jail(
    jail: &Path,
    socket: &mut String,
    disk: &mut Option<String>,
    uid: u32,
    gid: u32,
) -> io::Result<()> {
    let machine = jail.join("machine");
    let inside = |p: &str| -> io::Result<String> {
        let rest = Path::new(p)
            .strip_prefix(&machine)
            .map_err(|_| io::Error::other(format!("{p} is not under {}", machine.display())))?;
        Ok(Path::new(sandbox::MACHINE_DIR)
            .join(rest)
            .to_string_lossy()
            .into_owned())
    };
    let new_socket = inside(socket)?;
    let new_disk = disk.as_deref().map(inside).transpose()?;
    let alive = sandbox::enter(jail, &machine, uid, gid)?;
    thread::spawn(move || sandbox::exit_with_parent(alive));
    *socket = new_socket;
    *disk = new_disk;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn enter_jail(_: &Path, _: &mut String, _: &mut Option<String>, _: u32, _: u32) -> io::Result<()> {
    Err(io::Error::other("the VMM sandbox needs Linux"))
}

#[cfg(target_os = "linux")]
fn before_enter(jailed: bool) -> io::Result<()> {
    if jailed {
        sandbox::drop_groups()?;
        sandbox::install_filter(&sandbox::vmm_filter())?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn before_enter(_: bool) -> io::Result<()> {
    Ok(())
}

/// Copy a disk image, sharing blocks with the original where the filesystem
/// allows it (reflink), so a copy is cheap on XFS and btrfs.
fn copy_image(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let src = fs::File::open(from)?;
        let dst = fs::File::create(to)?;
        // SAFETY: FICLONE on two open files; failure falls back to a copy.
        let rc = unsafe {
            libc::ioctl(
                std::os::fd::AsRawFd::as_raw_fd(&dst),
                libc::FICLONE,
                std::os::fd::AsRawFd::as_raw_fd(&src),
            )
        };
        if rc == 0 {
            return Ok(());
        }
    }
    fs::copy(from, to).map(drop)
}

/// Give a machine directory and everything in it to the machine's uid.
#[cfg(target_os = "linux")]
fn chown_tree(dir: &Path, id: u32) -> io::Result<()> {
    std::os::unix::fs::chown(dir, Some(id), Some(id))?;
    for entry in fs::read_dir(dir)? {
        std::os::unix::fs::chown(entry?.path(), Some(id), Some(id))?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn chown_tree(_: &Path, _: u32) -> io::Result<()> {
    Ok(())
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
    fn capture<R: Read + Send + 'static>(&self, mut src: R) -> thread::JoinHandle<()> {
        let tail = self.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n @ 1..) = src.read(&mut buf) {
                let mut t = tail.0.lock().expect("console lock");
                t.extend(&buf[..n]);
                let excess = t.len().saturating_sub(CONSOLE_TAIL);
                t.drain(..excess);
            }
        })
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
///
/// The child sits behind a lock shared with the watchdog. Every wait and
/// every kill goes through `Child`, which refuses to signal a process it has
/// already reaped, so a recycled pid is never hit.
struct Machine(Arc<Mutex<Child>>);

impl Machine {
    fn try_wait(&self) -> io::Result<Option<std::process::ExitStatus>> {
        self.0.lock().expect("child lock").try_wait()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let mut child = self.0.lock().expect("child lock");
        let _ = child.kill();
        let _ = child.wait();
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
    fn start(child: Arc<Mutex<Child>>, deadline: Instant) -> Self {
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
                    let _ = child.lock().expect("child lock").kill();
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

/// Whether `run` can sandbox the VMM here: Linux, as root.
fn can_sandbox() -> bool {
    // SAFETY: geteuid has no preconditions.
    cfg!(target_os = "linux") && unsafe { libc::geteuid() } == 0
}

fn run(o: &Opts) -> io::Result<ExitCode> {
    if o.argv.is_empty() {
        return Err(io::Error::other("no command given after --"));
    }
    let scratch = Scratch::new()?;
    let machine = scratch.0.join("machine");
    fs::DirBuilder::new().mode(0o700).create(&machine)?;
    let socket = machine.join("agent.sock");

    let started = Instant::now();
    let mut boot = Command::new(std::env::current_exe()?);
    boot.arg("boot");
    match (&o.root, &o.disk) {
        (Some(dir), None) => {
            boot.arg("--root").arg(fs::canonicalize(dir)?);
        }
        (None, Some(image)) => {
            // Each machine writes to its own copy; the base image stays clean.
            let copy = machine.join("root.img");
            copy_image(Path::new(image), &copy)?;
            boot.arg("--disk").arg(copy);
        }
        _ => return Err(io::Error::other("give exactly one of --root and --disk")),
    }
    if can_sandbox() {
        // One uid per machine; the host agent allocates these from M1 on.
        let id = 200_000 + std::process::id() % 100_000;
        chown_tree(&machine, id)?;
        boot.arg("--jail").arg(&scratch.0).args([
            "--uid",
            &id.to_string(),
            "--gid",
            &id.to_string(),
        ]);
    } else if cfg!(target_os = "linux") && std::env::var_os("SANDPIPER_UNSANDBOXED").is_none() {
        // Set SANDPIPER_UNSANDBOXED=1 to say this is known and wanted.
        eprintln!("sandpiper-vmm: not root, so the VMM is NOT sandboxed (development only)");
    }
    let (lifeline_r, lifeline_w) = io::pipe()?;
    let mut child = boot
        .arg("--socket")
        .arg(&socket)
        .args(["--cpus", &o.cpus.to_string(), "--mem", &o.mem.to_string()])
        .arg("--die-with-parent")
        .stdin(lifeline_r)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let console = ConsoleTail::default();
    let captures = [
        console.capture(child.stdout.take().expect("piped")),
        console.capture(child.stderr.take().expect("piped")),
    ];
    let machine = Machine(Arc::new(Mutex::new(child)));
    let watchdog = Watchdog::start(Arc::clone(&machine.0), started + BOOT_TIMEOUT);

    let outcome = exchange(o, &socket, &machine, &watchdog, started);
    watchdog.stop();
    // Destroy the machine, then let the console readers drain to EOF so a
    // report shows the VMM's last words.
    drop(machine);
    drop(lifeline_w);
    let dump_console = || {
        for c in captures {
            let _ = c.join();
        }
        console.dump();
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        // A failure caused by the watchdog's kill is a timeout, not an error.
        Err(_) if watchdog.fired() => Outcome::TimedOut,
        Err(e) => {
            dump_console();
            return Err(e);
        }
    };

    match outcome {
        Outcome::TimedOut => {
            eprintln!("sandpiper-vmm: timed out; machine destroyed");
            dump_console();
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
    machine: &Machine,
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
    machine: &Machine,
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
        if let Some(status) = machine.try_wait()? {
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
