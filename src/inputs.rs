//! Build inputs: where the workspace is, which files it holds, and what each one hashes to.
//!
//! The whole-build key has to be cheap to compute on a hit, so unchanged files are skipped with a
//! stat cache keyed by size, mtime and inode, the same signal cargo's fingerprints use. Anything
//! whose stat moved is re-hashed; a hit path never reads a file it has already seen unchanged.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};

use crate::hash::Digest;

const STAT_CACHE_VERSION: u32 = 1;

/// Files up to this size are read into memory instead of memory-mapped when hashing.
const SMALL_FILE: u64 = 128 << 10;

/// How close a file's mtime has to be to now for its cached digest to be distrusted. A file
/// rewritten inside the same clock tick keeps its size, mtime and inode, so a stat entry recorded
/// moments ago can already be stale (the racy-clean rule). Such a file is hashed again; once its
/// mtime is older than this, the stat entry is trustworthy.
const RACY: Duration = Duration::from_secs(2);

/// `<target>/blitz/statcache.json`. It lives in the target directory so a fresh checkout starts
/// with an empty cache and a retained target keeps its digests.
pub fn stat_cache_path(target_dir: &Path) -> PathBuf {
    target_dir.join("blitz").join("statcache.json")
}

/// An absolute path. `..` is only cleaned up lexically, which is enough because every path blitz
/// compares comes from the current directory or a walk of the workspace.
pub fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Absolute form of `path`, resolved against `base` when it is relative.
pub fn absolute_from(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        absolute(path)
    } else {
        absolute(&base.join(path))
    }
}

pub fn cwd() -> PathBuf {
    std::env::current_dir()
        .map(|d| absolute(&d))
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// The value of `--name value` or `--name=value`, if the arguments carry one.
pub fn flag_value(args: &[OsString], name: &str) -> Option<OsString> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let Some(text) = arg.to_str() else { continue };
        if text == name {
            return it.next().cloned();
        }
        if let Some(value) = text.strip_prefix(name).and_then(|rest| rest.strip_prefix('=')) {
            return Some(OsString::from(value));
        }
    }
    None
}

/// The workspace root for a build whose manifest search starts in `start`: the nearest ancestor
/// directory whose `Cargo.toml` declares `[workspace]`, else the nearest `Cargo.toml` at all.
/// Walking past a plain package manifest is deliberate: a member directory has no `[workspace]`,
/// but cargo still builds it as part of the root above it.
pub fn workspace_root(start: &Path) -> Option<PathBuf> {
    let start = absolute(start);
    let mut fallback: Option<PathBuf> = None;
    for dir in start.ancestors() {
        let manifest = dir.join("Cargo.toml");
        let Ok(text) = fs::read_to_string(&manifest) else {
            continue;
        };
        if has_workspace_table(&text) {
            return Some(dir.to_path_buf());
        }
        fallback.get_or_insert_with(|| dir.to_path_buf());
    }
    fallback
}

/// True when the manifest text opens a `[workspace]` or `[workspace.<table>]` section.
fn has_workspace_table(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        let Some(rest) = line.strip_prefix('[') else {
            return false;
        };
        if rest.starts_with('[') {
            return false;
        }
        let Some(name) = rest.split(']').next() else {
            return false;
        };
        let name = name.trim();
        name == "workspace" || name.starts_with("workspace.")
    })
}

/// Every input file of the build, relative to `root` and sorted.
///
/// The walk honours `.gitignore` like a checkout does, includes hidden files (`.cargo/config.toml`
/// is an input), and skips `.git` plus every directory blitz writes itself: the target directory and
/// the cache stores, which can sit inside the workspace.
pub fn walk(root: &Path, skips: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .require_git(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .ignore(true)
        .parents(true)
        .follow_links(false);
    let skips = inside(root, skips);
    builder.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        if entry.file_name() == OsStr::new(".git") {
            return false;
        }
        !skips.iter().any(|skip| skip == entry.path())
    });

    let mut out = Vec::new();
    for entry in builder.build() {
        let entry = entry.with_context(|| format!("walking {}", root.display()))?;
        if entry.depth() == 0 {
            continue;
        }
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            continue;
        }
        let path = entry.path();
        if kind.is_symlink() && !fs::metadata(path).is_ok_and(|m| m.is_file()) {
            // A symlinked directory or a dangling link hides inputs the walk cannot see. Refuse the
            // whole-build cache rather than risk a stale hit.
            bail!("{} is a symlink that does not point at a file", path.display());
        }
        let rel = path
            .strip_prefix(root)
            .with_context(|| format!("{} is outside {}", path.display(), root.display()))?;
        out.push(rel.to_path_buf());
    }
    out.sort();
    Ok(out)
}

/// Every `Cargo.toml` and `Cargo.lock` under `root`, found by a second walk with the ignore rules
/// switched off.
///
/// Cargo reads a manifest that an ignore rule hides whenever the workspace reaches it as a member,
/// through a `members` glob, or as a path dependency, and no dep-info file ever mentions a manifest.
/// Keying the visible manifests alone let an edit to a gitignored member (`members = ["vendor/*"]`)
/// hit a stale entry. Only these two file names are collected: everything else the tree holds is
/// either already walked or too expensive to hash on the hit path, and a source file an ignored tree
/// needs is caught as an extra input from the dep-info file of the unit that reads it.
///
/// A manifest that cannot be read is skipped rather than failing the key: cargo will report it when
/// it builds, and a cache problem must never turn into a failed build.
pub fn manifests(root: &Path, skips: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .require_git(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .ignore(false)
        .parents(false)
        .follow_links(false);
    let skips = inside(root, skips);
    builder.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        if entry.file_name() == OsStr::new(".git") {
            return false;
        }
        !skips.iter().any(|skip| skip == entry.path())
    });

    let mut out = Vec::new();
    for entry in builder.build() {
        let entry = entry.with_context(|| format!("walking {} for manifests", root.display()))?;
        if entry.depth() == 0 {
            continue;
        }
        let name = entry.file_name();
        if name != OsStr::new("Cargo.toml") && name != OsStr::new("Cargo.lock") {
            continue;
        }
        // A symlinked manifest counts as a file, and one that cannot be read is skipped: cargo
        // reports it when it builds.
        if !fs::metadata(entry.path()).is_ok_and(|meta| meta.is_file()) || fs::File::open(entry.path()).is_err() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .with_context(|| format!("{} is outside {}", entry.path().display(), root.display()))?;
        out.push(rel.to_path_buf());
    }
    out.sort();
    Ok(out)
}

/// The entries of `skips` that sit inside `root`, and never `root` itself: a walk that skipped its
/// own start would hash nothing. Each skip is returned both as given and canonicalised, because the
/// walk yields paths built from `root` while the cache store roots are canonicalised.
fn inside(root: &Path, skips: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for skip in skips {
        let literal = absolute(skip);
        let real = fs::canonicalize(skip).unwrap_or_else(|_| literal.clone());
        for form in [literal, real] {
            if form.starts_with(root) && form != root && !out.contains(&form) {
                out.push(form);
            }
        }
    }
    out
}

/// Removes `.` and `..` lexically, for comparing paths that do not exist yet. It never touches the
/// filesystem, so a symlinked `..` can differ from the real path; callers canonicalize first when
/// the path exists.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            part => out.push(part),
        }
    }
    out
}

/// The package roots inside the workspace: every directory holding a `Cargo.toml` the walk found,
/// plus the workspace root itself. Cargo resolves a build script's relative `rerun-if-changed`
/// paths against the package root, so these are the bases worth trying.
pub fn package_roots(root: &Path, files: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = vec![root.to_path_buf()];
    for rel in files {
        if rel.file_name() == Some(OsStr::new("Cargo.toml")) {
            out.push(match rel.parent() {
                Some(dir) => root.join(dir),
                None => root.to_path_buf(),
            });
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A package outside the workspace that a path dependency pulls in.
pub struct External {
    /// The label from the manifest entry (`../helper`), the same in every checkout, so the key
    /// never contains an absolute path.
    pub label: String,
    /// Where that entry resolves to now.
    pub dir: PathBuf,
    /// Every file under `dir`, relative to it.
    pub files: Vec<PathBuf>,
}

/// Every outside package reachable through `path` dependencies, walked depth first.
///
/// Cargo reads those manifests and build scripts, and no dep-info file mentions the manifest, so an
/// edit there would otherwise be invisible to the key. Following the dependencies transitively
/// costs a walk of each external crate, which is small next to the build it protects.
pub fn external_packages(root: &Path, files: &[PathBuf]) -> Result<Vec<External>> {
    let root_real = fs::canonicalize(root).unwrap_or_else(|_| normalize(root));
    let mut queue: Vec<(String, PathBuf)> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for rel in files {
        if rel.file_name() != Some(OsStr::new("Cargo.toml")) {
            continue;
        }
        let dir = root.join(rel).parent().unwrap_or(root).to_path_buf();
        collect_escaping(&root_real, &dir, "", &mut queue, &mut seen)?;
    }

    let mut out = Vec::new();
    while let Some((label, dir)) = queue.pop() {
        let walked = walk(&dir, &[dir.join("target")])?;
        for rel in &walked {
            if rel.file_name() == Some(OsStr::new("Cargo.toml")) {
                let sub = dir.join(rel).parent().unwrap_or(&dir).to_path_buf();
                let sub_label = format!("{label}/{}", rel.parent().unwrap_or(Path::new("")).display());
                collect_escaping(&root_real, &sub, &sub_label, &mut queue, &mut seen)?;
            }
        }
        out.push(External {
            label,
            dir,
            files: walked,
        });
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(out)
}

/// Queues the `path` dependencies of one manifest that resolve outside the workspace.
fn collect_escaping(
    root: &Path,
    manifest_dir: &Path,
    prefix: &str,
    queue: &mut Vec<(String, PathBuf)>,
    seen: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let manifest = manifest_dir.join("Cargo.toml");
    let Ok(text) = fs::read_to_string(&manifest) else {
        return Ok(());
    };
    for value in path_values(&text) {
        let dir = absolute_from(manifest_dir, Path::new(&value));
        let real = fs::canonicalize(&dir).unwrap_or_else(|_| normalize(&dir));
        if real.starts_with(root) || !seen.insert(real.clone()) {
            continue;
        }
        let label = if prefix.is_empty() {
            value
        } else {
            format!("{prefix}/{value}")
        };
        queue.push((label, real));
    }
    Ok(())
}

/// The values of `path = "..."` keys in a manifest, in both the table form
/// (`[dependencies.foo] path = ".."`) and the inline form (`foo = { path = ".." }`).
/// Keys outside dependency tables (`[[bin]] path`, `[lib] path`) point at files inside the package,
/// which the walk already covers.
fn path_values(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        let mut from = 0;
        while let Some(found) = line[from..].find("path") {
            let start = from + found;
            let after = &line[start + "path".len()..];
            let key_like = line[..start]
                .chars()
                .next_back()
                .is_none_or(|c| c == '{' || c == ',' || c.is_whitespace());
            let rest = after.trim_start();
            if key_like {
                if let Some(value) = rest.strip_prefix('=').and_then(quoted) {
                    out.push(value);
                }
            }
            from = start + "path".len();
        }
    }
    out
}

/// The first quoted string in `text`, if it starts with a quote.
fn quoted(text: &str) -> Option<String> {
    let text = text.trim_start();
    let quote = text.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let rest = &text[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// Digests of the files the walk found, cached by stat so unchanged files are never read.
pub struct StatCache {
    version: u32,
    entries: BTreeMap<String, Stat>,
    seen: BTreeSet<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
struct Stat {
    size: u64,
    mtime: u64,
    inode: u64,
    digest: Digest,
}

#[derive(Serialize, Deserialize)]
struct StatCacheFile {
    version: u32,
    entries: BTreeMap<String, Stat>,
}

impl StatCache {
    pub fn load(target_dir: &Path) -> StatCache {
        let fresh = StatCache {
            version: STAT_CACHE_VERSION,
            entries: BTreeMap::new(),
            seen: BTreeSet::new(),
        };
        let Ok(bytes) = fs::read(stat_cache_path(target_dir)) else {
            return fresh;
        };
        match serde_json::from_slice::<StatCacheFile>(&bytes) {
            Ok(file) if file.version == STAT_CACHE_VERSION => StatCache {
                version: file.version,
                entries: file.entries,
                seen: BTreeSet::new(),
            },
            _ => fresh,
        }
    }

    /// The digest of `base/rel`, re-hashed only when its size, mtime or inode moved. `label` keeps
    /// the cache keys of an external package apart from the workspace's own files.
    pub fn digest(&mut self, base: &Path, rel: &Path) -> Result<Digest> {
        self.digest_labeled(base, rel, "")
    }

    /// The digest of `base/rel`, keyed under `label`, for files outside the workspace root.
    pub fn digest_labeled(&mut self, base: &Path, rel: &Path, label: &str) -> Result<Digest> {
        let full = base.join(rel);
        let meta = fs::metadata(&full).with_context(|| format!("statting {}", full.display()))?;
        let mut key = key_of(rel);
        if !label.is_empty() {
            key = format!("{label}\u{1f}{key}");
        }
        let size = meta.len();
        let mtime = mtime_ns(&meta);
        let inode = inode(&meta);
        self.seen.insert(key.clone());
        // A file written within the last couple of seconds can be rewritten again without any stat
        // field moving, so its recorded digest proves nothing yet; hash it.
        if !racy(&meta)
            && let Some(stat) = self.entries.get(&key)
            && stat.size == size
            && stat.mtime == mtime
            && stat.inode == inode
        {
            return Ok(stat.digest);
        }
        // A small file costs less to read than to map, and most build inputs are small.
        let digest = if size <= SMALL_FILE {
            let bytes = fs::read(&full).with_context(|| format!("reading {}", full.display()))?;
            Digest::of_bytes(&bytes)
        } else {
            Digest::of_file(&full).with_context(|| format!("hashing {}", full.display()))?
        };
        self.entries.insert(
            key,
            Stat {
                size,
                mtime,
                inode,
                digest,
            },
        );
        Ok(digest)
    }

    /// Writes the cache, dropping entries for files this build did not see.
    pub fn save(&mut self, target_dir: &Path) -> Result<()> {
        self.entries.retain(|key, _| self.seen.contains(key));
        let path = stat_cache_path(target_dir);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = StatCacheFile {
            version: self.version,
            entries: self.entries.clone(),
        };
        let tmp = path.with_extension(format!("json.{}", std::process::id()));
        fs::write(&tmp, serde_json::to_vec(&file)?)?;
        fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        Ok(())
    }
}

/// Where cargo will put its output: `--target-dir`, then `CARGO_TARGET_DIR`, then `build.target-dir`
/// in a cargo config, then `<root>/target`.
pub fn target_dir(root: &Path, args: &[OsString]) -> PathBuf {
    let cwd = cwd();
    if let Some(value) = flag_value(args, "--target-dir") {
        return absolute_from(&cwd, Path::new(&value));
    }
    if let Some(value) = std::env::var_os("CARGO_TARGET_DIR").filter(|v| !v.is_empty()) {
        return absolute_from(&cwd, Path::new(&value));
    }
    for config in config_files(root) {
        if let Some(value) = config_target_dir(&config) {
            return absolute_from(root, Path::new(&value));
        }
    }
    root.join("target")
}

/// Cargo config files that can move the target directory, closest to the workspace first.
fn config_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for name in ["config.toml", "config"] {
        out.push(root.join(".cargo").join(name));
    }
    if let Some(home) = cargo_home() {
        for name in ["config.toml", "config"] {
            out.push(home.join(name));
        }
    }
    out.retain(|p| p.is_file());
    out
}

/// `build.target-dir` from a cargo config, if it sets one.
fn config_target_dir(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let mut in_build = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(section) = line.strip_prefix('[') {
            in_build = section.trim_end_matches(']').trim() == "build";
            continue;
        }
        if !in_build {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "target-dir" {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

pub fn cargo_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CARGO_HOME").filter(|h| !h.is_empty()) {
        return Some(absolute(Path::new(&home)));
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".cargo"))
}

/// The per-user cargo configs. They change how cargo runs without touching any workspace file.
pub fn user_cargo_configs() -> Vec<PathBuf> {
    let Some(home) = cargo_home() else {
        return Vec::new();
    };
    ["config.toml", "config"]
        .iter()
        .map(|name| home.join(name))
        .filter(|p| p.is_file())
        .collect()
}

/// The cargo binary that will drive the compiler: the one next to `rustc` when the toolchain ships
/// one, else `$CARGO`, else whatever is on `PATH`.
pub fn cargo_binary(rustc: &Path) -> Option<PathBuf> {
    if let Some(dir) = rustc.parent() {
        let next = dir.join("cargo");
        if next.is_file() {
            return Some(next);
        }
    }
    if let Some(cargo) = std::env::var_os("CARGO").filter(|c| !c.is_empty()) {
        let path = PathBuf::from(cargo);
        if path.is_file() {
            return Some(path);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("cargo"))
        .find(|p| p.is_file())
}

fn key_of(rel: &Path) -> String {
    let bytes = rel.as_os_str().as_encoded_bytes();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub fn mtime_ns(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// True when `meta`'s mtime is too close to now to tell later writes apart, including an mtime in
/// the future, which a clock step can produce.
fn racy(meta: &fs::Metadata) -> bool {
    let Ok(modified) = meta.modified() else {
        return true;
    };
    match SystemTime::now().duration_since(modified) {
        Ok(age) => age < RACY,
        Err(_) => true,
    }
}

#[cfg(unix)]
fn inode(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn inode(_meta: &fs::Metadata) -> u64 {
    0
}
