//! Build script for the end-to-end fixture.
//!
//! It watches a directory rather than a file, the shape a single file digest cannot represent, and
//! writes the listing it read into OUT_DIR so the script has to run for a build to be complete.

use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=watched");
    let mut names: Vec<String> = fs::read_dir("watched")
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let out = std::env::var("OUT_DIR").expect("OUT_DIR is set for a build script");
    fs::write(format!("{out}/watched.txt"), names.join(",")).expect("write the watched listing");
}
