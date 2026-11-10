//! The guest-facing decoder (DESIGN.md §4.2). It runs as a thread of the
//! VMM process, so on Linux it is inside the VMM's sandbox: its own uid,
//! namespaces and seccomp filter. It is the only code that reads the
//! guest's raw frames.
//!
//! The relay listens on [`HOST_SOCKET`] in the machine's directory. Each
//! connection from the host is one exchange: the relay opens a connection
//! to the guest agent, waits for `Ready`, and then passes frames both ways.
//! Frames from the guest are checked before they go on:
//!
//! - the first must be `Ready` with this protocol version;
//! - after that only `Stdout`, `Stderr`, `Error` and `Exit` may come, and
//!   `Exit` ends the exchange;
//! - output past the per-stream cap is dropped, and the cut is marked in
//!   `Exit`; `Error` text is cut to [`MAX_ERROR`] bytes.
//!
//! Anything else ends the exchange, so the host sees no `Exit`.
//!
//! The host still decodes the relay's frames, with the same fuzzed decoder.
//! What the relay adds: a decoder bug is first reached here, inside the
//! sandbox, and the host only ever reads frames that passed these checks
//! unless the sandbox itself has been taken over.

use std::fs;
use std::io;
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use sandpiper_proto::{
    DEFAULT_OUTPUT_CAP, ExitStatus, Frame, PROTOCOL_VERSION, read_frame, write_frame,
};

/// The relay's socket, in the machine's directory.
pub const HOST_SOCKET: &str = "host.sock";

/// Longest `Error` text passed to the host, in bytes.
pub const MAX_ERROR: usize = 4096;

/// Start the relay thread. `machine` and `guest` are paths as this process
/// sees them. With `uid`, the relay waits until the process runs as that
/// uid before it listens, so the host's peer check sees the machine's uid.
pub fn start(machine: PathBuf, guest: PathBuf, uid: Option<u32>) {
    thread::spawn(move || {
        if let Err(e) = serve(&machine, &guest, uid) {
            eprintln!("sandpiper-vmm: relay stopped: {e}");
        }
    });
}

fn serve(machine: &Path, guest: &Path, uid: Option<u32>) -> io::Result<()> {
    if let Some(uid) = uid {
        // SAFETY: geteuid has no preconditions. libkrun switches the uid of
        // every thread just before the guest starts.
        while unsafe { libc::geteuid() } != uid {
            thread::sleep(Duration::from_millis(1));
        }
    }
    let path = machine.join(HOST_SOCKET);
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    // The guest agent serves one exchange at a time, and so does the relay.
    for host in listener.incoming() {
        if let Err(e) = exchange(host?, guest) {
            eprintln!("sandpiper-vmm: relay: {e}");
        }
    }
    Ok(())
}

fn exchange(mut host: UnixStream, guest_path: &Path) -> io::Result<()> {
    let Some(mut guest) = connect_guest(guest_path, &host)? else {
        return Ok(()); // the host left first
    };
    write_frame(
        &mut host,
        &Frame::Ready {
            version: PROTOCOL_VERSION,
        },
    )?;

    // Host to guest: the host is trusted, but only requests go this way.
    let mut from_host = host.try_clone()?;
    let mut to_guest = guest.try_clone()?;
    let forward = thread::spawn(move || {
        while let Ok(Some(frame)) = read_frame(&mut from_host) {
            if !matches!(frame, Frame::Exec(_) | Frame::Stdin(_))
                || write_frame(&mut to_guest, &frame).is_err()
            {
                break;
            }
        }
        // The host is gone or done talking: hang up on the guest too, which
        // kills the command if it still runs.
        let _ = to_guest.shutdown(Shutdown::Both);
    });

    let result = pass_output(&mut guest, &mut host);
    let _ = guest.shutdown(Shutdown::Both);
    let _ = host.shutdown(Shutdown::Both);
    let _ = forward.join();
    result
}

/// Guest to host, checked. Returns once `Exit` has gone to the host.
fn pass_output(guest: &mut UnixStream, host: &mut UnixStream) -> io::Result<()> {
    let mut caps = OutputCaps::default();
    loop {
        let frame = read_frame(guest)?
            .ok_or_else(|| io::Error::other("guest closed the connection before Exit"))?;
        match caps.check(frame)? {
            Some(frame @ Frame::Exit(_)) => return write_frame(host, &frame),
            Some(frame) => write_frame(host, &frame)?,
            None => {}
        }
    }
}

/// Per-exchange output limits.
#[derive(Default)]
struct OutputCaps {
    stdout: usize,
    stderr: usize,
    stdout_cut: bool,
    stderr_cut: bool,
}

impl OutputCaps {
    /// The frame to pass on, if any, or an error for a frame the guest may
    /// not send.
    fn check(&mut self, frame: Frame) -> io::Result<Option<Frame>> {
        let cap = |used: &mut usize, cut: &mut bool, mut b: Vec<u8>| {
            let room = DEFAULT_OUTPUT_CAP.saturating_sub(*used);
            if b.len() > room {
                *cut = true;
                b.truncate(room);
            }
            *used += b.len();
            b
        };
        Ok(match frame {
            Frame::Stdout(b) => {
                let b = cap(&mut self.stdout, &mut self.stdout_cut, b);
                (!b.is_empty()).then_some(Frame::Stdout(b))
            }
            Frame::Stderr(b) => {
                let b = cap(&mut self.stderr, &mut self.stderr_cut, b);
                (!b.is_empty()).then_some(Frame::Stderr(b))
            }
            Frame::Error(mut msg) => {
                if msg.len() > MAX_ERROR {
                    let mut end = MAX_ERROR;
                    while !msg.is_char_boundary(end) {
                        end -= 1;
                    }
                    msg.truncate(end);
                }
                Some(Frame::Error(msg))
            }
            Frame::Exit(s) => Some(Frame::Exit(ExitStatus {
                code: s.code,
                stdout_truncated: s.stdout_truncated || self.stdout_cut,
                stderr_truncated: s.stderr_truncated || self.stderr_cut,
            })),
            other => {
                return Err(io::Error::other(format!(
                    "guest sent a {} frame mid-exchange",
                    other.kind()
                )));
            }
        })
    }
}

/// Connect to the guest agent and read its `Ready`, retrying while the
/// machine boots. libkrun owns the listening socket, so a connect can
/// succeed before the guest listens; only `Ready` proves the agent is
/// there. Gives up, returning `None`, when the host hangs up.
fn connect_guest(path: &Path, host: &UnixStream) -> io::Result<Option<UnixStream>> {
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
        if hung_up(host) {
            return Ok(None);
        }
        match UnixStream::connect(path) {
            Ok(mut conn) => {
                conn.set_read_timeout(Some(Duration::from_millis(500)))?;
                match read_frame(&mut conn) {
                    Ok(Some(Frame::Ready { version })) if version == PROTOCOL_VERSION => {
                        conn.set_read_timeout(None)?;
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

/// Whether the peer has closed its end, without reading from the stream.
fn hung_up(s: &UnixStream) -> bool {
    let mut pfd = libc::pollfd {
        fd: s.as_raw_fd(),
        events: 0,
        revents: 0,
    };
    // SAFETY: poll on a descriptor we own, with a zero timeout.
    unsafe { libc::poll(&mut pfd, 1, 0) > 0 && pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_cut_output_and_mark_exit() {
        let mut c = OutputCaps::default();
        let full = c
            .check(Frame::Stdout(vec![1; DEFAULT_OUTPUT_CAP - 1]))
            .unwrap();
        assert!(matches!(full, Some(Frame::Stdout(b)) if b.len() == DEFAULT_OUTPUT_CAP - 1));
        let cut = c.check(Frame::Stdout(vec![2; 10])).unwrap();
        assert_eq!(cut, Some(Frame::Stdout(vec![2])));
        assert_eq!(c.check(Frame::Stdout(vec![3])).unwrap(), None);
        let exit = c
            .check(Frame::Exit(ExitStatus {
                code: 0,
                stdout_truncated: false,
                stderr_truncated: false,
            }))
            .unwrap();
        assert_eq!(
            exit,
            Some(Frame::Exit(ExitStatus {
                code: 0,
                stdout_truncated: true,
                stderr_truncated: false,
            }))
        );
    }

    #[test]
    fn error_text_is_cut_on_a_char_boundary() {
        let mut c = OutputCaps::default();
        let long = "é".repeat(MAX_ERROR);
        match c.check(Frame::Error(long)).unwrap() {
            Some(Frame::Error(m)) => assert!(m.len() <= MAX_ERROR && m.len() >= MAX_ERROR - 1),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn guest_may_not_send_requests() {
        let mut c = OutputCaps::default();
        assert!(c.check(Frame::Stdin(vec![])).is_err());
        assert!(c.check(Frame::Ready { version: 1 }).is_err());
    }
}
