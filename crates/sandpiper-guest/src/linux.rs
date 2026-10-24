use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use sandpiper_proto::{
    DEFAULT_OUTPUT_CAP, ExecRequest, ExitStatus, Frame, GUEST_PORT, PROTOCOL_VERSION, read_frame,
    write_frame,
};

const CHUNK: usize = 32 * 1024;
/// After the command exits, an output stream that has been idle this long
/// is held open only by something the command left running in the
/// background. Its output after that point is not the command's.
const PUMP_GRACE: Duration = Duration::from_millis(200);
const POLL: Duration = Duration::from_millis(5);
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

type Writer = Arc<Mutex<File>>;

pub fn serve() -> io::Result<()> {
    let listener = vsock_listen(GUEST_PORT)?;
    loop {
        let conn = vsock_accept(&listener)?;
        // One exchange at a time is enough for M0; the host serialises.
        if let Err(e) = handle(conn) {
            eprintln!("sandpiper-guest: connection failed: {e}");
        }
        reap_orphans();
    }
}

fn vsock_listen(port: u32) -> io::Result<OwnedFd> {
    // SAFETY: plain socket syscalls; every return value is checked and the
    // fd is owned by OwnedFd from the moment it exists.
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = port;
        addr.svm_cid = libc::VMADDR_CID_ANY;
        let rc = libc::bind(
            std::os::fd::AsRawFd::as_raw_fd(&fd),
            (&addr as *const libc::sockaddr_vm).cast(),
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        );
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::listen(std::os::fd::AsRawFd::as_raw_fd(&fd), 4) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
}

fn vsock_accept(listener: &OwnedFd) -> io::Result<File> {
    loop {
        // SAFETY: accept4 on a listening socket we own; the result is checked.
        let fd = unsafe {
            libc::accept4(
                std::os::fd::AsRawFd::as_raw_fd(listener),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if fd >= 0 {
            // SAFETY: fd is a fresh descriptor returned by accept4.
            return Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }));
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// One exchange. Threads:
/// - a **reader** owns the connection's read side: it forwards stdin frames,
///   then keeps reading so a host hang-up is noticed at any point;
/// - a **stdin writer** feeds the child, so a child that never reads stdin
///   cannot block anything else;
/// - two **pumps** forward stdout and stderr.
///
/// This thread only watches: the child's exit, a host hang-up, the pumps.
fn handle(conn: File) -> io::Result<()> {
    let mut reader = conn.try_clone()?;
    let writer: Writer = Arc::new(Mutex::new(conn));
    send(
        &writer,
        &Frame::Ready {
            version: PROTOCOL_VERSION,
        },
    )?;

    let req = match read_frame(&mut reader)? {
        Some(Frame::Exec(req)) => req,
        Some(other) => {
            return Err(io::Error::other(format!(
                "expected Exec, got {}",
                other.kind()
            )));
        }
        None => return Ok(()),
    };

    let mut child = match command(&req).spawn() {
        Ok(child) => child,
        Err(e) => {
            send(
                &writer,
                &Frame::Error(format!("cannot start {:?}: {e}", req.argv.first())),
            )?;
            let code = if e.kind() == io::ErrorKind::NotFound {
                127
            } else {
                126
            };
            return send(
                &writer,
                &Frame::Exit(ExitStatus {
                    code,
                    stdout_truncated: false,
                    stderr_truncated: false,
                }),
            );
        }
    };

    let hangup = Arc::new(AtomicBool::new(false));
    let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
    spawn_reader(reader, stdin_tx, Arc::clone(&hangup));
    spawn_stdin_writer(child.stdin.take().expect("piped"), stdin_rx);
    let out = Pump::spawn(child.stdout.take().expect("piped"), &writer, Frame::Stdout);
    let err = Pump::spawn(child.stderr.take().expect("piped"), &writer, Frame::Stderr);

    let status = match wait_or_hangup(&mut child, &hangup)? {
        Some(status) => status,
        None => {
            out.abandon();
            err.abandon();
            return Err(io::Error::other("host hung up; command killed"));
        }
    };
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));

    // Exit must not overtake output. Wait until each stream has ended, or
    // has sat idle in `read` (not mid-send) for the grace period.
    let since_exit = Instant::now();
    loop {
        if hangup.load(Ordering::Relaxed) {
            out.abandon();
            err.abandon();
            return Err(io::Error::other("host hung up before Exit"));
        }
        let settled = |p: &Pump| p.finished() || p.idle_for() >= PUMP_GRACE;
        if since_exit.elapsed() >= PUMP_GRACE && settled(&out) && settled(&err) {
            break;
        }
        if out.finished() && err.finished() {
            break;
        }
        thread::sleep(POLL);
    }
    let stdout_truncated = out.finish()?;
    let stderr_truncated = err.finish()?;
    send(
        &writer,
        &Frame::Exit(ExitStatus {
            code,
            stdout_truncated,
            stderr_truncated,
        }),
    )
}

/// Wait for the child, killing it if the host hangs up first. Polls so that
/// this thread alone owns the child: no other thread can reap it, so `kill`
/// never hits a recycled pid.
fn wait_or_hangup(
    child: &mut Child,
    hangup: &AtomicBool,
) -> io::Result<Option<std::process::ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if hangup.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(POLL);
    }
}

fn spawn_reader(mut reader: File, stdin_tx: mpsc::Sender<Vec<u8>>, hangup: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut stdin_tx = Some(stdin_tx);
        loop {
            match read_frame(&mut reader) {
                Ok(Some(Frame::Stdin(data))) if stdin_tx.is_some() => {
                    if data.is_empty() {
                        stdin_tx = None; // end of input: the writer closes the pipe
                    } else if let Some(tx) = &stdin_tx {
                        let _ = tx.send(data);
                    }
                }
                // End of stream, an error, or anything unexpected: the host
                // is gone or broken. Either way the exchange is over.
                _ => {
                    hangup.store(true, Ordering::Relaxed);
                    return;
                }
            }
        }
    });
}

/// Feed the child's stdin until the reader signals end of input. A child that
/// exits early makes writes fail; the rest of its input is dropped.
fn spawn_stdin_writer(mut stdin: ChildStdin, rx: mpsc::Receiver<Vec<u8>>) {
    thread::spawn(move || {
        for data in rx {
            if stdin.write_all(&data).is_err() {
                return;
            }
        }
    });
}

/// Forwards one output stream as frames, stopping at the cap but reading on
/// so the child never blocks on a full pipe.
struct Pump {
    state: Arc<Mutex<PumpState>>,
}

struct PumpState {
    /// Taken away on abandon, so a pump left blocked in `read` by a
    /// background process holds no reference to the connection.
    writer: Option<Writer>,
    sending: bool,
    last_activity: Instant,
    /// Set when the stream ends: whether the cap was hit, or the error.
    result: Option<io::Result<bool>>,
}

impl Pump {
    fn spawn<R: Read + Send + 'static>(
        src: R,
        writer: &Writer,
        wrap: fn(Vec<u8>) -> Frame,
    ) -> Self {
        let state = Arc::new(Mutex::new(PumpState {
            writer: Some(Arc::clone(writer)),
            sending: false,
            last_activity: Instant::now(),
            result: None,
        }));
        let s = Arc::clone(&state);
        thread::spawn(move || {
            let r = forward(src, &s, wrap);
            let mut st = s.lock().expect("pump lock");
            st.result = Some(r);
            st.writer = None;
        });
        Self { state }
    }

    fn finished(&self) -> bool {
        self.state.lock().expect("pump lock").result.is_some()
    }

    /// How long the pump has been waiting in `read`; zero while it is sending.
    fn idle_for(&self) -> Duration {
        let st = self.state.lock().expect("pump lock");
        if st.sending {
            Duration::ZERO
        } else {
            st.last_activity.elapsed()
        }
    }

    fn abandon(&self) {
        self.state.lock().expect("pump lock").writer = None;
    }

    /// The pump's result if it ended; otherwise abandon it. An abandoned
    /// stream is not marked truncated: what it would still have carried came
    /// from a background process after the command exited.
    fn finish(self) -> io::Result<bool> {
        let mut st = self.state.lock().expect("pump lock");
        st.writer = None;
        st.result.take().unwrap_or(Ok(false))
    }
}

fn forward<R: Read>(
    mut src: R,
    state: &Mutex<PumpState>,
    wrap: fn(Vec<u8>) -> Frame,
) -> io::Result<bool> {
    let mut sent = 0usize;
    let mut truncated = false;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => return Ok(truncated),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let room = DEFAULT_OUTPUT_CAP.saturating_sub(sent);
        if n > room {
            truncated = true;
        }
        let take = n.min(room);
        if take == 0 {
            continue;
        }
        let writer = {
            let mut st = state.lock().expect("pump lock");
            match st.writer.clone() {
                Some(w) => {
                    st.sending = true;
                    w
                }
                None => continue, // abandoned: drain and discard
            }
        };
        let res = send(&writer, &wrap(buf[..take].to_vec()));
        drop(writer);
        let mut st = state.lock().expect("pump lock");
        st.sending = false;
        st.last_activity = Instant::now();
        drop(st);
        res?;
        sent += take;
    }
}

fn command(req: &ExecRequest) -> Command {
    let (program, args) = req
        .argv
        .split_first()
        .map_or(("", &[][..]), |(p, a)| (p.as_str(), a));
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .env("PATH", DEFAULT_PATH)
        .envs(req.env.iter().map(|(k, v)| (k, v)))
        .current_dir(req.cwd.as_deref().unwrap_or("/"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Reap orphaned processes. The agent may be PID 1, so anything a command
/// left behind is reparented here and must be waited for.
fn reap_orphans() {
    // SAFETY: waitpid with WNOHANG on any child; called only between
    // exchanges, when no `Child` handle of ours is outstanding.
    while unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0 {}
}

fn send(writer: &Mutex<File>, frame: &Frame) -> io::Result<()> {
    let mut w = writer.lock().expect("writer lock");
    write_frame(&mut *w, frame)
}
