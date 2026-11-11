//! sandpiper's own nftables table (DESIGN.md §7.3), Linux only. It is the
//! second layer behind "no network device": traffic to or from a machine's
//! interface (named `sp-…`, from M2) is refused, so a device attached by
//! mistake still reaches nothing. No other table is touched.
//!
//! The host agent installs the table at start and compares it with what it
//! installed on a timer. If another program changed or flushed it, the host
//! agent stops starting machines (DESIGN.md §4.2).

use std::io::{self, Write};
use std::process::{Command, Stdio};

const TABLE: &str = r#"table inet sandpiper {
    chain input {
        type filter hook input priority filter - 10; policy accept;
        iifname "sp-*" counter reject
    }
    chain forward {
        type filter hook forward priority filter - 10; policy accept;
        iifname "sp-*" counter reject
        oifname "sp-*" counter reject
    }
    chain output {
        type filter hook output priority filter - 10; policy accept;
        oifname "sp-*" counter reject
    }
}
"#;

/// Replace sandpiper's table, in one transaction, and return its listing,
/// once it is known to hold exactly the rules above: the listing becomes
/// the baseline the watch compares with, so it must not be taken from a
/// table another program changed in between.
pub fn install() -> io::Result<String> {
    // Adding first makes the delete safe when the table does not exist yet.
    let script = format!("add table inet sandpiper\ndelete table inet sandpiper\n{TABLE}");
    let mut nft = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| io::Error::other(format!("nft: {e} (install nftables)")))?;
    nft.stdin
        .take()
        .expect("piped")
        .write_all(script.as_bytes())?;
    let out = nft.wait_with_output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "nft could not install the sandpiper table: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let listed = listing()?;
    if normalize(&listed) != normalize(TABLE) {
        return Err(io::Error::other(format!(
            "the sandpiper table does not hold the rules just installed:\n{listed}"
        )));
    }
    Ok(listed)
}

/// The table's lines without indentation or blank lines.
fn normalize(table: &str) -> Vec<&str> {
    table
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect()
}

/// The table as nft lists it now, without counter values, which change.
pub fn listing() -> io::Result<String> {
    let out = Command::new("nft")
        .args(["-s", "list", "table", "inet", "sandpiper"])
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "the sandpiper table is gone: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
