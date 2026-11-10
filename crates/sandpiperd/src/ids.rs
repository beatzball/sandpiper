//! Random identifiers and secrets.

use std::fs::File;
use std::io::Read;

/// `bytes` random bytes from the kernel, as lowercase hex.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("the kernel's random source is readable");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// A new opaque ID with a type prefix, such as `m_3f9c…`.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", random_hex(8))
}

/// Whether `s` could be an ID this host made: the prefix and 16 hex digits.
/// IDs become path names, so nothing else is accepted.
pub fn is_id(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix)
        .and_then(|r| r.strip_prefix('_'))
        .is_some_and(|hex| {
            hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_reject_paths() {
        let id = new_id("m");
        assert!(is_id(&id, "m"));
        assert!(!is_id(&id, "e"));
        assert!(!is_id("m_../../etc/passwd", "m"));
        assert!(!is_id("m_0123456789ABCDEF", "m"));
        assert_ne!(new_id("m"), new_id("m"));
    }
}
