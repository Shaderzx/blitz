//! Per-unit cache: blitz as `RUSTC_WRAPPER`.
//!
//! IMPLEMENTATION OWNER: the per-unit worker. Contract (see docs/design.md, "Per-unit cache"):
//! - `main` must never fail a build because of the cache. Any internal error means: run the real
//!   compiler with the original arguments and return its exit code.
//! - Only cacheable invocations are cached; everything else is passed straight through.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

/// True when `arg` is a compiler path, i.e. blitz was started by cargo as `RUSTC_WRAPPER`.
/// A bare `rustc` only counts inside cargo (which sets `CARGO_PKG_NAME` for the compiler), so
/// `blitz rustc ...` typed by a user still means the `cargo rustc` subcommand.
pub fn looks_like_compiler(arg: &OsStr) -> bool {
    let p = Path::new(arg);
    let name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let compiler = name == "rustc" || name == "clippy-driver" || name.ends_with("-rustc");
    compiler && (p.components().count() > 1 || std::env::var_os("CARGO_PKG_NAME").is_some())
}

pub fn main(rustc: &OsStr, args: &[OsString]) -> i32 {
    passthrough(rustc, args)
}

/// Runs the real compiler with the original arguments, inheriting stdio.
pub fn passthrough(rustc: &OsStr, args: &[OsString]) -> i32 {
    match Command::new(rustc).args(args).status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("blitz: cannot run {}: {e}", Path::new(rustc).display());
            1
        }
    }
}
