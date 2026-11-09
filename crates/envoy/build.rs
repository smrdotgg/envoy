//! Linux release builds fold identical functions and constants (lld's `--icf=safe`), which
//! generic Rust code has plenty of: the shipped binary gets smaller (docs/NORTH_STAR.md: < 30 MB)
//! and nothing changes at run time. `safe` folds only what an address-significance table says
//! has no meaningful address: rustc writes one when asked (`-Cllvm-args=-addrsig`, in
//! .cargo/config.toml), where Rust functions and promoted constants have none and statics do.
//! An object without a table, such as the C code (SQLite, ring), is never folded, so its
//! function-pointer comparisons keep working. Rust links x86_64 Linux with its bundled lld;
//! other targets and debug builds are left alone (Windows' and macOS' linkers fold by default).

fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let release = std::env::var("PROFILE").is_ok_and(|p| p == "release");
    if os == "linux" && env == "gnu" && arch == "x86_64" && release {
        println!("cargo:rustc-link-arg-bins=-Wl,--icf=safe");
    }
}
