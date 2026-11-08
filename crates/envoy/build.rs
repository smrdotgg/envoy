//! Linux release builds fold identical functions (lld's `--icf=safe`), which generic Rust code
//! has plenty of: the shipped binary gets smaller (docs/NORTH_STAR.md: < 30 MB) and nothing
//! changes at run time. `safe` only folds functions whose address is never taken, so pointer
//! comparisons keep working. Rust links x86_64 Linux with its bundled lld; other targets and
//! debug builds are left alone.

fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let release = std::env::var("PROFILE").is_ok_and(|p| p == "release");
    if os == "linux" && env == "gnu" && arch == "x86_64" && release {
        println!("cargo:rustc-link-arg-bins=-Wl,--icf=safe");
    }
}
