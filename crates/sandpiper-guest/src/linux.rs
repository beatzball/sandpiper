use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use sandpiper_proto::{
    DEFAULT_OUTPUT_CAP, ExecRequest, ExitStatus, Frame, GUEST_PORT, PROTOCOL_VERSION, read_frame,
    write_frame,
};

const CHUNK: usize = 32 * 1024;
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub fn serve() -> io::Result<()> {
    let listener = vsock_listen(GUEST_PORT)?;
    loop {
        let conn = vsock_accept(&listener)?;
        // One exchange at a time is enough for M0; the host serialises.
        if let Err(e) = handle(conn) {
            eprintln!("sandpiper-guest: connection failed: {e}");
        }
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

fn handle(conn: File) -> io::Result<()> {
    let mut reader = conn.try_clone()?;
    let writer = Arc::new(Mutex::new(conn));
    send(
        &writer,
        &Frame::Ready {
            version: PROTOCOL_VERSION,
        },
    )?;

    let req = match read_frame(&mut reader)? {
        Some(Frame::Exec(req)) => req,
        Some(other) => return Err(io::Error::other(format!("expected Exec, got {other:?}"))),
        None => return Ok(()),
    };

    let mut child = match command(&req).spawn() {
        Ok(child) => child,
        Err(e) => {
            send(
                &writer,
                &Frame::Error(format!("cannot start {:?}: {e}", req.argv.first())),
            )?;
            drain_stdin(&mut reader)?;
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

    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let out = pump(stdout, Arc::clone(&writer), Frame::Stdout);
    let err = pump(stderr, Arc::clone(&writer), Frame::Stderr);

    // Feed stdin until the host sends the empty end-of-input frame. A child
    // that exits early makes writes fail; that is not an error here.
    let mut stdin = child.stdin.take();
    loop {
        match read_frame(&mut reader)? {
            Some(Frame::Stdin(data)) if data.is_empty() => break,
            Some(Frame::Stdin(data)) => {
                if let Some(s) = stdin.as_mut()
                    && s.write_all(&data).is_err()
                {
                    stdin = None;
                }
            }
            Some(other) => return Err(io::Error::other(format!("expected Stdin, got {other:?}"))),
            None => break,
        }
    }
    drop(stdin);

    let stdout_truncated = out.join().expect("pump thread")?;
    let stderr_truncated = err.join().expect("pump thread")?;
    let status = child.wait()?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    send(
        &writer,
        &Frame::Exit(ExitStatus {
            code,
            stdout_truncated,
            stderr_truncated,
        }),
    )
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

/// Forward a child's output stream as frames until EOF. Stops forwarding at
/// the cap but keeps reading, so the child never blocks on a full pipe.
/// Returns whether the cap was hit.
fn pump<R: Read + Send + 'static>(
    mut src: R,
    writer: Arc<Mutex<File>>,
    wrap: fn(Vec<u8>) -> Frame,
) -> thread::JoinHandle<io::Result<bool>> {
    thread::spawn(move || {
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
            if take > 0 {
                send(&writer, &wrap(buf[..take].to_vec()))?;
                sent += take;
            }
        }
    })
}

fn drain_stdin(reader: &mut File) -> io::Result<()> {
    while let Some(frame) = read_frame(reader)? {
        if matches!(&frame, Frame::Stdin(d) if d.is_empty()) {
            break;
        }
    }
    Ok(())
}

fn send(writer: &Mutex<File>, frame: &Frame) -> io::Result<()> {
    let mut w = writer.lock().expect("writer lock");
    write_frame(&mut *w, frame)
}
