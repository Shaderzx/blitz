//! blitz: a cargo front-end that never compiles the same thing twice.
//!
//! Two caches share one content-addressed store (see `store`):
//! - whole-build (`whole`): `blitz build` hashes every input of the build up front; on a hit it
//!   restores the finished artifacts without starting cargo at all.
//! - per-unit (`wrapper`): on a miss, cargo runs with blitz as `RUSTC_WRAPPER`, and every cacheable
//!   rustc invocation is answered from the store when its inputs were compiled before.

mod buildscript;
mod cli;
mod depinfo;
mod file_metadata;
mod hash;
mod incremental;
mod inputs;
mod normalize;
mod store;
mod toolchain;
mod whole;
mod wrapper;

use std::ffi::OsString;

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    // Cargo runs `$RUSTC_WRAPPER $RUSTC_WORKSPACE_WRAPPER $RUSTC <args...>` when both wrappers are
    // set, so the first argument can be another wrapper that blitz must simply run.
    if let Some(code) = wrapper::chained(&args[1..]) {
        std::process::exit(code);
    }
    // Cargo runs `$RUSTC_WRAPPER <path to rustc> <rustc args...>`.
    if args.len() >= 2 && wrapper::looks_like_compiler(&args[1]) {
        std::process::exit(wrapper::main(&args[1], &args[2..]));
    }
    std::process::exit(cli::main(&args[1..]));
}
