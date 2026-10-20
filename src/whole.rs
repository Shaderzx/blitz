//! Whole-build cache: `blitz build`.
//!
//! IMPLEMENTATION OWNER: the whole-build worker. Contract (see docs/design.md, "Whole-build cache"):
//! - Hit: restore the recorded artifacts into the target directory without running cargo.
//! - Miss: run `cargo build` (with blitz as RUSTC_WRAPPER), record the artifacts, store the entry.
//! - Anything blitz cannot model safely falls back to plain `cargo build` with the same arguments.

use std::ffi::OsString;

use anyhow::Result;

pub fn build(args: &[OsString]) -> Result<i32> {
    crate::cli::run_cargo("build", args, true)
}
