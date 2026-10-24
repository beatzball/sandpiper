// Link libkrun. KRUN_LIB_DIR overrides the search path; on macOS the
// Homebrew prefix is tried by default.
fn main() {
    println!("cargo:rerun-if-env-changed=KRUN_LIB_DIR");
    if let Ok(dir) = std::env::var("KRUN_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
    } else if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
    }
    println!("cargo:rustc-link-lib=dylib=krun");
}
