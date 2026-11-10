//! Feed arbitrary bytes to the frame decoder, as a hostile guest would.
//! Run with `cargo +nightly fuzz run read_frame` from `crates/sandpiper-proto`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use sandpiper_proto::{read_frame, write_frame};

fuzz_target!(|data: &[u8]| {
    let mut r = data;
    while let Ok(Some(frame)) = read_frame(&mut r) {
        // Whatever decodes must encode again and decode to the same frame.
        let mut buf = Vec::new();
        if write_frame(&mut buf, &frame).is_ok() {
            assert_eq!(read_frame(&mut buf.as_slice()).unwrap(), Some(frame));
        }
    }
});
