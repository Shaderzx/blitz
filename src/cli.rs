//! Command-line entry: `blitz build`, `blitz <any cargo subcommand>`, `blitz stats`, `blitz gc`.

use std::ffi::OsString;
use std::process::Command;

use anyhow::{Context, Result};

use crate::store::Store;

const USAGE: &str = "\
blitz: a cargo front-end with a whole-build cache and a per-crate cache

USAGE:
    blitz build [CARGO BUILD ARGS]   build, restoring the whole result from cache when nothing changed
    blitz <cargo subcommand> [ARGS]  run any cargo subcommand with the per-crate cache (test, check, clippy, run...)
    blitz stats                      show cache hits and misses
    blitz gc --max-size <SIZE>       trim the cache to SIZE (e.g. 20G), oldest first
    blitz cache-dir                  print the cache directories

ENVIRONMENT:
    BLITZ_CACHE_DIR   writable cache directory (default ~/.cache/blitz)
    BLITZ_CACHE_RO    extra read-only cache directories, separated by ':'
    BLITZ_READONLY=1  never write to the cache
    BLITZ_DISABLE=1   run plain cargo, no caching
    BLITZ_LOG=<file>  append a line per cache decision to <file>
";

pub fn main(args: &[OsString]) -> i32 {
    match dispatch(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("blitz: {e:#}");
            1
        }
    }
}

fn dispatch(args: &[OsString]) -> Result<i32> {
    let Some(cmd) = args.first().and_then(|a| a.to_str()) else {
        print!("{USAGE}");
        return Ok(2);
    };
    let rest = &args[1..];
    match cmd {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(0)
        }
        "-V" | "--version" => {
            println!("blitz {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        "build" | "b" if std::env::var("BLITZ_DISABLE").is_ok_and(|v| v == "1") => run_cargo("build", rest, false),
        "build" | "b" => crate::whole::build(rest),
        "stats" => stats(),
        "cache-dir" => {
            for (root, writable) in Store::from_env()?.roots() {
                println!(
                    "{} ({})",
                    root.display(),
                    if writable { "read-write" } else { "read-only" }
                );
            }
            Ok(0)
        }
        "gc" => gc(rest),
        other => {
            let wrap = !std::env::var("BLITZ_DISABLE").is_ok_and(|v| v == "1");
            run_cargo(other, rest, wrap)
        }
    }
}

/// Runs `cargo <sub> <args>`, inheriting stdio. With `wrap`, blitz is set as `RUSTC_WRAPPER`
/// unless the caller already set one.
pub fn run_cargo(sub: &str, args: &[OsString], wrap: bool) -> Result<i32> {
    let mut cmd = Command::new(cargo_bin());
    cmd.arg(sub).args(args);
    if wrap && std::env::var_os("RUSTC_WRAPPER").is_none_or(|w| w.is_empty()) {
        cmd.env(
            "RUSTC_WRAPPER",
            std::env::current_exe().context("locating the blitz binary")?,
        );
    }
    let status = cmd.status().context("running cargo")?;
    Ok(status.code().unwrap_or(1))
}

pub fn cargo_bin() -> OsString {
    std::env::var_os("CARGO")
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "cargo".into())
}

fn stats() -> Result<i32> {
    let store = Store::from_env()?;
    let Some(root) = store.writable_root() else {
        println!("no writable cache");
        return Ok(0);
    };
    let text = std::fs::read_to_string(root.join("stats.log")).unwrap_or_default();
    let mut counts: std::collections::BTreeMap<&str, u64> = Default::default();
    for line in text.lines() {
        if let Some(event) = line.split_whitespace().nth(1) {
            *counts.entry(event).or_default() += 1;
        }
    }
    println!("cache: {}", root.display());
    for (event, n) in counts {
        println!("{event:>16} {n}");
    }
    Ok(0)
}

fn gc(args: &[OsString]) -> Result<i32> {
    let max = args
        .iter()
        .position(|a| a == "--max-size")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.to_str())
        .context("usage: blitz gc --max-size <SIZE>")?;
    let limit = parse_size(max).context("bad size; use a number with an optional K, M or G suffix")?;
    let store = Store::from_env()?;
    let root = store.writable_root().context("no writable cache")?;
    let mut files = Vec::new();
    for sub in ["cas", "ac"] {
        collect(&root.join(sub), &mut files);
    }
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort_by_key(|f| f.2);
    let mut removed = 0u64;
    for (path, len, _) in files {
        if total <= limit {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
            removed += 1;
        }
    }
    println!("removed {removed} files; cache is now {} MiB", total / (1 << 20));
    Ok(0)
}

fn collect(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, u64, std::time::SystemTime)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(m) = e.metadata() else { continue };
        if m.is_dir() {
            collect(&e.path(), out);
        } else {
            out.push((e.path(), m.len(), m.modified().unwrap_or(std::time::UNIX_EPOCH)));
        }
    }
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        'K' | 'k' => (&s[..s.len() - 1], 1u64 << 10),
        'M' | 'm' => (&s[..s.len() - 1], 1 << 20),
        'G' | 'g' => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    num.parse::<u64>().ok().map(|n| n * mult)
}
