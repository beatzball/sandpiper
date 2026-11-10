// Link libkrun. KRUN_LIB_DIR overrides the search path; by default the
// Homebrew prefix is tried on macOS, and /usr/local/lib64 (where
// scripts/install-libkrun-linux.sh puts it) on Linux.
fn main() {
    println!("cargo:rerun-if-env-changed=KRUN_LIB_DIR");
    if let Ok(dir) = std::env::var("KRUN_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
    } else if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
    } else if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-search=native=/usr/local/lib64");
    }
    println!("cargo:rustc-link-lib=dylib=krun");
}
