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
/// Stdin frames queued between the connection and the child, at most.
const STDIN_QUEUE: usize = 8;
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
    // Bounded, so a child that reads slowly pushes back on the host instead
    // of the guest buffering all of its input in memory.
    let (stdin_tx, stdin_rx) = mpsc::sync_channel::<Vec<u8>>(STDIN_QUEUE);
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
    // has sat blocked in `read` for the grace period (then it is abandoned).
    let since_exit = Instant::now();
    loop {
        if hangup.load(Ordering::Relaxed) {
            out.abandon();
            err.abandon();
            return Err(io::Error::other("host hung up before Exit"));
        }
        if out.finished() && err.finished() {
            break;
        }
        if since_exit.elapsed() >= PUMP_GRACE {
            // Evaluate both every round: settling one may abandon it.
            let (o, e) = (out.settle(), err.settle());
            if o && e {
                break;
            }
        }
        thread::sleep(POLL);
    }
    let (stdout, stderr) = (out.finish(), err.finish());
    let (stdout_truncated, stderr_truncated) = (stdout?, stderr?);
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

fn spawn_reader(mut reader: File, stdin_tx: mpsc::SyncSender<Vec<u8>>, hangup: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut stdin_tx = Some(stdin_tx);
        loop {
            match read_frame(&mut reader) {
                Ok(Some(Frame::Stdin(data))) if stdin_tx.is_some() => {
                    if data.is_empty() {
                        stdin_tx = None; // end of input: the writer closes the pipe
                    } else if let Some(tx) = &stdin_tx
                        && tx.send(data).is_err()
                    {
                        // The child stopped taking input; drop the rest but
                        // keep reading so a hang-up is still noticed.
                        stdin_tx = None;
                    }
                }
                Ok(Some(Frame::Stdin(_))) => {} // input after the child stopped reading
                // Known limit: while blocked on a full queue (a live child
                // that is not reading stdin) a hang-up is not seen until the
                // child exits. The host destroys the VM in M0; M1 replaces
                // this with poll(2).
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
    /// True while the pump is blocked in `read` with nothing in hand.
    in_read: bool,
    /// When the pump last finished a read or a send.
    last_activity: Instant,
    truncated: bool,
    /// Set when the stream ends; `Err` if reading or sending failed.
    result: Option<io::Result<()>>,
}

impl Pump {
    fn spawn<R: Read + Send + 'static>(
        src: R,
        writer: &Writer,
        wrap: fn(Vec<u8>) -> Frame,
    ) -> Self {
        let state = Arc::new(Mutex::new(PumpState {
            writer: Some(Arc::clone(writer)),
            in_read: false,
            last_activity: Instant::now(),
            truncated: false,
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

    /// True once the stream has ended, or — checked and acted on under one
    /// lock — has been blocked in `read` for the grace period, in which case
    /// it is abandoned. A pump holding data or mid-send is never abandoned.
    ///
    /// Residual window: a pump whose `read` returned data but which has not
    /// yet retaken the lock still reads as blocked. That data arrived at least
    /// the grace period after the command exited, so it came from something
    /// the command left running, and dropping it is the documented behaviour.
    fn settle(&self) -> bool {
        let mut st = self.state.lock().expect("pump lock");
        if st.result.is_some() {
            return true;
        }
        if st.in_read && st.last_activity.elapsed() >= PUMP_GRACE {
            st.writer = None;
            return true;
        }
        false
    }

    fn abandon(&self) {
        self.state.lock().expect("pump lock").writer = None;
    }

    /// Whether the stream hit the cap. An abandoned stream keeps the flag it
    /// had earned before it was abandoned.
    fn finish(self) -> io::Result<bool> {
        let mut st = self.state.lock().expect("pump lock");
        st.writer = None;
        match st.result.take() {
            Some(Err(e)) => Err(e),
            _ => Ok(st.truncated),
        }
    }
}

fn forward<R: Read>(
    mut src: R,
    state: &Mutex<PumpState>,
    wrap: fn(Vec<u8>) -> Frame,
) -> io::Result<()> {
    let mut sent = 0usize;
    let mut buf = vec![0u8; CHUNK];
    loop {
        state.lock().expect("pump lock").in_read = true;
        let read = src.read(&mut buf);
        let writer = {
            let mut st = state.lock().expect("pump lock");
            st.in_read = false;
            st.last_activity = Instant::now();
            let n = match read {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            let take = n.min(DEFAULT_OUTPUT_CAP.saturating_sub(sent));
            if take < n {
                st.truncated = true;
            }
            match (&st.writer, take) {
                (Some(w), 1..) => {
                    sent += take;
                    Some((Arc::clone(w), take))
                }
                _ => None, // over the cap, or abandoned: drain and discard
            }
        };
        if let Some((w, take)) = writer {
            let res = send(&w, &wrap(buf[..take].to_vec()));
            drop(w);
            state.lock().expect("pump lock").last_activity = Instant::now();
            res?;
        }
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
