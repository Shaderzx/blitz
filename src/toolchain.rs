//! Toolchain identity: the `rustc -vV` text for a compiler binary, cached by the binary's metadata
//! so the common path costs one `stat` instead of a process spawn.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::hash::{Digest, KeyBuilder};

/// `rustc -vV` output for `rustc`, cached under `<cache>/toolchains/` by path, size, mtime and inode.
pub fn rustc_identity(rustc: &Path, cache_root: Option<&Path>) -> Result<String> {
    let meta = fs::metadata(rustc).with_context(|| format!("stat {}", rustc.display()))?;
    let mut k = KeyBuilder::new("blitz-toolchain-v1");
    k.part("path", rustc.as_os_str().as_encoded_bytes());
    k.part("len", &meta.len().to_le_bytes());
    if let Ok(m) = meta.modified() {
        let nanos = m
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        k.part("mtime", &nanos.to_le_bytes());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        k.part("ino", &meta.ino().to_le_bytes());
    }
    let key = k.finish();
    let cached = cache_root.map(|r| r.join("toolchains").join(format!("{}.txt", key.hex())));
    if let Some(p) = &cached {
        if let Ok(s) = fs::read_to_string(p) {
            return Ok(s);
        }
    }
    let out = Command::new(rustc)
        .arg("-vV")
        .output()
        .with_context(|| format!("running {} -vV", rustc.display()))?;
    if !out.status.success() {
        bail!("{} -vV failed", rustc.display());
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if let Some(p) = cached {
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
            let tmp = dir.join(format!(".{}.{}", key.short(), std::process::id()));
            if fs::write(&tmp, &text).is_ok() {
                let _ = fs::rename(&tmp, &p);
            }
        }
    }
    Ok(text)
}

/// Digest of the toolchain identity, for use in cache keys.
pub fn rustc_digest(rustc: &Path, cache_root: Option<&Path>) -> Result<Digest> {
    Ok(Digest::of_bytes(rustc_identity(rustc, cache_root)?.as_bytes()))
}

/// The real compiler cargo would use in `dir`, found without spawning rustup when possible:
/// `$RUSTC`, else the rustup toolchain picked by `$RUSTUP_TOOLCHAIN`, a `rust-toolchain(.toml)`
/// file in `dir` or an ancestor, or rustup's default; else `rustc` on `PATH`.
/// Falls back to `rustup which rustc` when the toolchain directory cannot be worked out.
pub fn resolve_rustc(dir: &Path) -> Result<PathBuf> {
    if let Some(r) = std::env::var_os("RUSTC").filter(|r| !r.is_empty()) {
        return Ok(which(Path::new(&r)).unwrap_or_else(|| PathBuf::from(r)));
    }
    if let Some(home) = rustup_home() {
        if home.join("toolchains").is_dir() {
            if let Some(name) = toolchain_name(dir, &home) {
                if let Some(p) = toolchain_rustc(&home, &name) {
                    return Ok(p);
                }
            }
            let out = Command::new("rustup")
                .args(["which", "rustc"])
                .current_dir(dir)
                .output();
            if let Ok(out) = out {
                if out.status.success() {
                    return Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()));
                }
            }
        }
    }
    which(Path::new("rustc")).context("rustc not found on PATH")
}

fn rustup_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("RUSTUP_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(h));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".rustup"))
}

fn toolchain_name(dir: &Path, home: &Path) -> Option<String> {
    if let Ok(t) = std::env::var("RUSTUP_TOOLCHAIN") {
        if !t.is_empty() {
            return Some(t);
        }
    }
    for d in dir.ancestors() {
        for file in ["rust-toolchain.toml", "rust-toolchain"] {
            if let Ok(text) = fs::read_to_string(d.join(file)) {
                return parse_toolchain_file(&text);
            }
        }
    }
    let settings = fs::read_to_string(home.join("settings.toml")).ok()?;
    // Directory overrides (`rustup override set`) win over the default.
    let mut in_overrides = false;
    let mut default = None;
    for line in settings.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_overrides = line == "[overrides]";
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim().trim_matches('"'), v.trim().trim_matches('"'));
        if in_overrides {
            if dir.starts_with(k) {
                return Some(v.to_string());
            }
        } else if k == "default_toolchain" {
            default = Some(v.to_string());
        }
    }
    default
}

fn parse_toolchain_file(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !trimmed.contains('=') && !trimmed.contains('[') {
        return Some(trimmed.to_string());
    }
    for line in trimmed.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "channel" {
                return Some(v.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

fn toolchain_rustc(home: &Path, name: &str) -> Option<PathBuf> {
    let toolchains = home.join("toolchains");
    let direct = toolchains.join(name).join("bin").join("rustc");
    if direct.is_file() {
        return Some(direct);
    }
    // `stable` is installed as `stable-<host triple>`.
    let prefix = format!("{name}-");
    let mut matches: Vec<PathBuf> = fs::read_dir(&toolchains)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .map(|e| e.path().join("bin").join("rustc"))
        .filter(|p| p.is_file())
        .collect();
    if matches.len() == 1 { matches.pop() } else { None }
}

fn which(name: &Path) -> Option<PathBuf> {
    if name.components().count() > 1 {
        return name.is_file().then(|| name.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}
