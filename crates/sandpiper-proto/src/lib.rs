//! Wire protocol between the host and the guest agent (DESIGN.md §4.4).
//!
//! A frame is a big-endian `u32` length, then one type byte, then the
//! payload. The length counts the type byte and the payload. Frames are
//! capped at [`MAX_FRAME`] so a hostile guest cannot make the host allocate
//! without bound.
//!
//! One connection carries one exchange:
//!
//! ```text
//! guest → host   Ready(version)
//! host  → guest  Exec(request)
//! host  → guest  Stdin(bytes)*  Stdin(empty)      empty = end of input
//! guest → host   Stdout(bytes)* Stderr(bytes)*    interleaved
//! guest → host   Error(text)?                     only if the command could not start
//! guest → host   Exit(status)
//! ```

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// The vsock port the guest agent listens on.
pub const GUEST_PORT: u32 = 1024;

/// Bumped whenever a frame's meaning changes.
pub const PROTOCOL_VERSION: u16 = 1;

/// Largest payload a frame may carry, in bytes.
pub const MAX_FRAME: usize = 1 << 20;

/// Default cap on each output stream of one exec, in bytes.
pub const DEFAULT_OUTPUT_CAP: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    /// The process exit code, or 128 + signal number if it was killed.
    pub code: i32,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Ready { version: u16 },
    Exec(ExecRequest),
    Stdin(Vec<u8>),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(ExitStatus),
    Error(String),
}

const T_READY: u8 = 1;
const T_EXEC: u8 = 2;
const T_STDIN: u8 = 3;
const T_STDOUT: u8 = 4;
const T_STDERR: u8 = 5;
const T_EXIT: u8 = 6;
const T_ERROR: u8 = 7;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

impl Frame {
    /// The frame's type name, safe to log: it never includes guest data.
    pub fn kind(&self) -> &'static str {
        match self {
            Frame::Ready { .. } => "Ready",
            Frame::Exec(_) => "Exec",
            Frame::Stdin(_) => "Stdin",
            Frame::Stdout(_) => "Stdout",
            Frame::Stderr(_) => "Stderr",
            Frame::Exit(_) => "Exit",
            Frame::Error(_) => "Error",
        }
    }

    fn encode_payload(&self) -> io::Result<(u8, Vec<u8>)> {
        Ok(match self {
            Frame::Ready { version } => (T_READY, version.to_be_bytes().to_vec()),
            Frame::Exec(req) => (T_EXEC, serde_json::to_vec(req).map_err(io::Error::other)?),
            Frame::Stdin(b) => (T_STDIN, b.clone()),
            Frame::Stdout(b) => (T_STDOUT, b.clone()),
            Frame::Stderr(b) => (T_STDERR, b.clone()),
            Frame::Exit(s) => {
                let mut p = s.code.to_be_bytes().to_vec();
                p.push(u8::from(s.stdout_truncated) | (u8::from(s.stderr_truncated) << 1));
                (T_EXIT, p)
            }
            Frame::Error(msg) => (T_ERROR, msg.as_bytes().to_vec()),
        })
    }

    fn decode_payload(kind: u8, p: Vec<u8>) -> io::Result<Frame> {
        Ok(match kind {
            T_READY => {
                let b: [u8; 2] = p.try_into().map_err(|_| invalid("bad Ready payload"))?;
                Frame::Ready {
                    version: u16::from_be_bytes(b),
                }
            }
            T_EXEC => Frame::Exec(serde_json::from_slice(&p).map_err(|e| invalid(e.to_string()))?),
            T_STDIN => Frame::Stdin(p),
            T_STDOUT => Frame::Stdout(p),
            T_STDERR => Frame::Stderr(p),
            T_EXIT => {
                let b: [u8; 5] = p.try_into().map_err(|_| invalid("bad Exit payload"))?;
                Frame::Exit(ExitStatus {
                    code: i32::from_be_bytes([b[0], b[1], b[2], b[3]]),
                    stdout_truncated: b[4] & 1 != 0,
                    stderr_truncated: b[4] & 2 != 0,
                })
            }
            T_ERROR => Frame::Error(String::from_utf8_lossy(&p).into_owned()),
            other => return Err(invalid(format!("unknown frame type {other}"))),
        })
    }
}

/// Write one frame. Fails without writing if the payload exceeds [`MAX_FRAME`].
pub fn write_frame<W: Write>(w: &mut W, frame: &Frame) -> io::Result<()> {
    let (kind, payload) = frame.encode_payload()?;
    if payload.len() > MAX_FRAME {
        return Err(invalid(format!(
            "frame payload of {} bytes exceeds the cap",
            payload.len()
        )));
    }
    let len = u32::try_from(payload.len() + 1).expect("bounded by MAX_FRAME");
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.push(kind);
    buf.extend_from_slice(&payload);
    w.write_all(&buf)?;
    w.flush()
}

/// Read one frame. Returns `Ok(None)` on a clean end of stream before a
/// frame starts; a stream that ends mid-frame is an error.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    loop {
        match r.read(&mut len[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            // Nothing consumed yet, so retrying cannot desync the stream.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    r.read_exact(&mut len[1..])?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len - 1 > MAX_FRAME {
        return Err(invalid(format!("frame length {len} out of range")));
    }
    let mut kind = [0u8; 1];
    r.read_exact(&mut kind)?;
    let mut payload = vec![0u8; len - 1];
    r.read_exact(&mut payload)?;
    Frame::decode_payload(kind[0], payload).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(f: Frame) {
        let mut buf = Vec::new();
        write_frame(&mut buf, &f).unwrap();
        let mut r = buf.as_slice();
        assert_eq!(read_frame(&mut r).unwrap(), Some(f));
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn every_frame_round_trips() {
        round_trip(Frame::Ready {
            version: PROTOCOL_VERSION,
        });
        round_trip(Frame::Exec(ExecRequest {
            argv: vec!["echo".into(), "hello".into()],
            env: vec![("A".into(), "b".into())],
            cwd: Some("/work".into()),
        }));
        round_trip(Frame::Stdin(b"in".to_vec()));
        round_trip(Frame::Stdin(Vec::new()));
        round_trip(Frame::Stdout(b"out".to_vec()));
        round_trip(Frame::Stderr(vec![0, 255]));
        round_trip(Frame::Exit(ExitStatus {
            code: -1,
            stdout_truncated: true,
            stderr_truncated: false,
        }));
        round_trip(Frame::Error("no such file".into()));
    }

    #[test]
    fn rejects_oversized_length_without_allocating() {
        let mut buf = (u32::MAX).to_be_bytes().to_vec();
        buf.push(T_STDOUT);
        assert!(read_frame(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn rejects_zero_length() {
        let buf = 0u32.to_be_bytes();
        assert!(read_frame(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn rejects_unknown_type() {
        let buf = [0, 0, 0, 1, 99];
        assert!(read_frame(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn truncated_frame_is_an_error_not_eof() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Frame::Stdout(b"hello".to_vec())).unwrap();
        buf.truncate(buf.len() - 2);
        assert!(read_frame(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn refuses_to_write_oversized_payload() {
        let mut buf = Vec::new();
        assert!(write_frame(&mut buf, &Frame::Stdout(vec![0; MAX_FRAME + 1])).is_err());
        assert!(buf.is_empty());
    }

    #[test]
    fn bad_fixed_size_payloads_are_errors() {
        assert!(Frame::decode_payload(T_READY, vec![1]).is_err());
        assert!(Frame::decode_payload(T_EXIT, vec![0; 4]).is_err());
    }
}
