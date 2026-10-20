//! Whole-build cache: `blitz build`.
//!
//! The key covers every file in the workspace, the toolchain, the cargo configuration, the
//! arguments and the environment that can change output bytes. A hit restores the recorded
//! artifacts and never starts cargo; a miss runs cargo with the per-unit wrapper and records what
//! it produced. Anything blitz cannot model falls back to plain `cargo build` with the same
//! arguments, because a wrong cache answer is far worse than a slow build.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::hash::{Digest, KeyBuilder};
use crate::inputs::{self, StatCache};
use crate::store::Store;
use crate::toolchain;

const NAMESPACE: &str = "whole";
const ENTRY_VERSION: u32 = 1;
const KEY_DOMAIN: &str = "blitz-whole-v1";

/// Variables whose value can change output bytes, on top of everything prefixed `CARGO_`.
const KEY_ENV: &[&str] = &[
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTC_BOOTSTRAP",
    "RUSTUP_TOOLCHAIN",
    "CC",
    "CXX",
    "AR",
    "CFLAGS",
    "CXXFLAGS",
    "LDFLAGS",
    "PKG_CONFIG_PATH",
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
];

/// `CARGO_*` variables that do not change what is built: where the cache lives, how many jobs run,
/// and how the console looks.
const KEY_ENV_SKIP: &[&str] = &["CARGO_HOME", "CARGO_TARGET_DIR", "CARGO_MAKEFLAGS", "CARGO_BUILD_JOBS"];

pub fn build(args: &[OsString]) -> Result<i32> {
    if std::env::var("BLITZ_DISABLE").is_ok_and(|v| v == "1") {
        return crate::cli::run_cargo("build", args, true);
    }
    let started = Instant::now();
    match cached(args, started) {
        Ok(code) => Ok(code),
        Err(e) => {
            eprintln!("blitz: whole-build cache skipped: {e:#}");
            crate::cli::run_cargo("build", args, true)
        }
    }
}

fn cached(args: &[OsString], started: Instant) -> Result<i32> {
    if let Some(reason) = unsupported(args) {
        bail!("{reason}");
    }
    let store = Store::from_env()?;
    let root = workspace_for(args)?;
    let target_dir = inputs::target_dir(&root, args);
    let (key, mut statcache, files, externals) = key_of(&store, &root, &target_dir, args)?;

    if let Some(entry) = load_hit(&store, &key) {
        match restore(&store, &target_dir, &entry) {
            Ok(()) => {
                let _ = statcache.save(&target_dir);
                let ms = started.elapsed().as_millis();
                eprintln!("    Finished `{}` profile [restored by blitz] in {ms}ms", entry.profile);
                store.record("whole-hit", &key.short());
                return Ok(0);
            }
            Err(e) => eprintln!("blitz: restoring the cached build failed ({e:#}); rebuilding"),
        }
    }
    let build = Build {
        root: &root,
        target_dir: &target_dir,
        files: &files,
        externals: &externals,
    };
    run_and_record(&store, args, &build, &key, &mut statcache)
}

/// Arguments blitz does not model. Each of them changes what cargo reports or where it writes, so
/// the whole-build cache cannot stand in for the build; plain cargo runs instead.
fn unsupported(args: &[OsString]) -> Option<String> {
    for arg in args {
        let text = arg.to_string_lossy();
        let flag = match text.split_once('=') {
            Some((flag, _)) => flag,
            None => text.as_ref(),
        };
        let reason = match flag {
            "--message-format" => "--message-format is the caller's to control",
            "--build-plan" => "--build-plan is unstable",
            "--unit-graph" => "--unit-graph is unstable",
            "--out-dir" | "--artifact-dir" => "--out-dir/--artifact-dir write outside the target directory",
            "--timings" => "--timings reports a build blitz would not run",
            _ if flag.starts_with("-Z") => "-Z unstable flags are not modeled",
            _ => continue,
        };
        return Some(format!("{reason} ({flag})"));
    }
    None
}

fn workspace_for(args: &[OsString]) -> Result<PathBuf> {
    let cwd = inputs::cwd();
    let root = inputs::workspace_root(&cwd).context("no cargo workspace around the current directory")?;
    if let Some(manifest) = inputs::flag_value(args, "--manifest-path") {
        let manifest = inputs::absolute_from(&cwd, Path::new(&manifest));
        if !manifest.starts_with(&root) {
            bail!("--manifest-path points outside {}", root.display());
        }
    }
    Ok(root)
}

fn key_of(
    store: &Store,
    root: &Path,
    target_dir: &Path,
    args: &[OsString],
) -> Result<(Digest, StatCache, Vec<PathBuf>, Vec<inputs::External>)> {
    let cache_root = store.writable_root().map(Path::to_path_buf);
    let mut key = KeyBuilder::new(KEY_DOMAIN);

    let files = inputs::walk(root, target_dir)?;
    let mut statcache = StatCache::load(target_dir);
    key.part("file-count", &(files.len() as u64).to_le_bytes());
    for rel in &files {
        key.part("file-path", rel.as_os_str().as_encoded_bytes());
        key.digest("file-digest", &statcache.digest(root, rel)?);
    }

    // Path dependencies outside the workspace: cargo reads their manifests and build scripts, and
    // no dep-info file mentions the manifest, so their files are part of the key too.
    let externals = inputs::external_packages(root, &files)?;
    key.part("external-count", &(externals.len() as u64).to_le_bytes());
    for external in &externals {
        key.part("external-label", external.label.as_bytes());
        key.part("external-files", &(external.files.len() as u64).to_le_bytes());
        for rel in &external.files {
            key.part("external-path", rel.as_os_str().as_encoded_bytes());
            key.digest(
                "external-digest",
                &statcache.digest_labeled(&external.dir, rel, &external.label)?,
            );
        }
    }

    // The user's cargo config changes the build without touching a workspace file.
    for config in inputs::user_cargo_configs() {
        key.digest("cargo-config", &Digest::of_file(&config)?);
    }

    // The toolchain: `rustc -vV` from the identity cache, plus the cargo that will drive it.
    let rustc = toolchain::resolve_rustc(root)?;
    key.part(
        "rustc",
        toolchain::rustc_identity(&rustc, cache_root.as_deref())?.as_bytes(),
    );
    match inputs::cargo_binary(&rustc).and_then(|cargo| fs::metadata(cargo).ok()) {
        Some(meta) => {
            key.part("cargo-len", &meta.len().to_le_bytes());
            key.part("cargo-mtime", &inputs::mtime_ns(&meta).to_le_bytes());
        }
        None => {
            key.part("cargo", b"unresolved");
        }
    }

    for arg in filtered_args(args) {
        key.part("arg", arg.as_os_str().as_encoded_bytes());
    }

    for (name, value) in key_env() {
        key.part("env-name", name.as_os_str().as_encoded_bytes());
        key.part("env-value", value.as_os_str().as_encoded_bytes());
    }

    key.part("os", std::env::consts::OS.as_bytes());
    key.part("arch", std::env::consts::ARCH.as_bytes());
    Ok((key.finish(), statcache, files, externals))
}

/// The arguments that can change the build. Flags that only change the console, and the number of
/// jobs, are dropped: they change how long a build takes, not what it produces.
fn filtered_args(args: &[OsString]) -> Vec<OsString> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let text = args[i].to_string_lossy();
        match text.as_ref() {
            "-q" | "--quiet" | "-v" | "-vv" | "--verbose" | "--keep-going" => i += 1,
            "--color" | "-j" | "--jobs" => i += 2,
            // The target directory changes where output lands, not what the build produces; the
            // entry stores artifact paths relative to it, so a restore works in another one too.
            "--target-dir" => i += 2,
            _ if text.starts_with("--color=") || text.starts_with("--jobs=") || text.starts_with("--target-dir=") => {
                i += 1
            }
            _ if text.starts_with("-j") && text[2..].chars().all(|c| c.is_ascii_digit()) => i += 1,
            _ => {
                out.push(args[i].clone());
                i += 1;
            }
        }
    }
    out
}

/// The environment variables that are part of the key, sorted so the order of `environ` is
/// irrelevant.
fn key_env() -> Vec<(OsString, OsString)> {
    let extra: Vec<String> = std::env::var("BLITZ_KEY_ENV")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    for (name, value) in std::env::vars_os() {
        let Some(text) = name.to_str() else { continue };
        let keep = (text.starts_with("CARGO_") && !KEY_ENV_SKIP.contains(&text) && !text.starts_with("CARGO_TERM_"))
            || KEY_ENV.contains(&text)
            || extra.iter().any(|e| e == text);
        if keep {
            out.push((name, value));
        }
    }
    out.sort();
    out
}

fn load_hit(store: &Store, key: &Digest) -> Option<Entry> {
    let entry: Entry = store.get_entry(NAMESPACE, key)?;
    if entry.version != ENTRY_VERSION || entry.artifacts.is_empty() {
        return None;
    }
    if !store.has_all(entry.artifacts.iter().map(|a| &a.digest)) {
        return None;
    }
    if !extra_inputs_unchanged(&entry.extra) {
        return None;
    }
    Some(entry)
}

fn extra_inputs_unchanged(extra: &[ExtraInput]) -> bool {
    extra.iter().all(|input| match input {
        ExtraInput::File { path, digest } => Digest::of_file(Path::new(path)).ok() == *digest,
        ExtraInput::Env { name, value } => std::env::var(name).ok() == *value,
    })
}

fn restore(store: &Store, target_dir: &Path, entry: &Entry) -> Result<()> {
    for artifact in &entry.artifacts {
        let dest = target_dir.join(&artifact.path);
        store.materialize(&artifact.digest, &dest, if artifact.executable { 0o755 } else { 0o644 })?;
    }
    Ok(())
}

fn run_and_record(
    store: &Store,
    args: &[OsString],
    build: &Build,
    key: &Digest,
    statcache: &mut StatCache,
) -> Result<i32> {
    let mut full: Vec<OsString> = args.to_vec();
    let end = full.iter().position(|a| a == "--").unwrap_or(full.len());
    full.insert(end, OsString::from("--message-format=json-render-diagnostics"));

    let mut cmd = Command::new(crate::cli::cargo_bin());
    cmd.arg("build").args(&full);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if std::env::var_os("RUSTC_WRAPPER").is_none_or(|w| w.is_empty())
        && !std::env::var("BLITZ_NO_WRAPPER").is_ok_and(|v| v == "1")
    {
        cmd.env(
            "RUSTC_WRAPPER",
            std::env::current_exe().context("locating the blitz binary")?,
        );
    }

    let mut child = cmd.spawn().context("running cargo")?;
    let mut artifacts: Vec<PathBuf> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        for line in std::io::BufReader::new(stdout).split(b'\n') {
            let Ok(line) = line else { break };
            collect_artifacts(&line, build.root, &mut artifacts);
        }
    }
    let status = child.wait().context("waiting for cargo")?;
    let code = status.code().unwrap_or(1);
    if code != 0 {
        store.record("whole-miss", &format!("{} failed", key.short()));
        return Ok(code);
    }
    if let Err(e) = record(store, build, &profile_name(args), key, statcache, &artifacts) {
        eprintln!("blitz: could not record the whole-build entry: {e:#}");
    }
    Ok(code)
}

/// Reads one line of cargo's JSON output and keeps the artifacts of workspace packages.
fn collect_artifacts(line: &[u8], root: &Path, out: &mut Vec<PathBuf>) {
    if line.first() != Some(&b'{') {
        return;
    }
    let Ok(msg) = serde_json::from_slice::<serde_json::Value>(line) else {
        return;
    };
    if msg.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
        return;
    }
    let Some(manifest) = msg.get("manifest_path").and_then(|v| v.as_str()) else {
        return;
    };
    // Dependencies come from the registry or git and are pinned by Cargo.lock, so their artifacts
    // never need restoring; only the workspace's own outputs end up in the entry.
    if !Path::new(manifest).starts_with(root) {
        return;
    }
    if let Some(files) = msg.get("filenames").and_then(|v| v.as_array()) {
        for file in files {
            if let Some(path) = file.as_str() {
                out.push(PathBuf::from(path));
            }
        }
    }
    if let Some(exe) = msg.get("executable").and_then(|v| v.as_str()) {
        out.push(PathBuf::from(exe));
    }
}

/// What a build reads and writes: the workspace root, the target directory it fills, the files the
/// walk found under the root, and the packages a path dependency pulls in from outside it.
struct Build<'a> {
    root: &'a Path,
    target_dir: &'a Path,
    files: &'a [PathBuf],
    externals: &'a [inputs::External],
}

fn record(
    store: &Store,
    build: &Build,
    profile: &str,
    key: &Digest,
    statcache: &mut StatCache,
    artifacts: &[PathBuf],
) -> Result<()> {
    let (root, target_dir, files) = (build.root, build.target_dir, build.files);
    let mut seen = BTreeSet::new();
    let mut stored = Vec::new();
    for path in artifacts {
        let Ok(rel) = path.strip_prefix(target_dir) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if !seen.insert(rel.clone()) {
            continue;
        }
        let Ok(meta) = fs::metadata(path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let digest = store.put_file(path)?;
        stored.push(Artifact {
            path: rel,
            digest,
            executable: is_executable(&meta),
        });
    }
    if stored.is_empty() {
        // Nothing was produced inside the target directory. An entry that restores nothing would
        // answer the next build with a hit that leaves the user without a binary.
        return Ok(());
    }
    stored.sort_by(|a, b| a.path.cmp(&b.path));
    // Build scripts inside an external package resolve their relative paths against that package,
    // so its root is a base too.
    let mut bases = inputs::package_roots(root, files);
    bases.extend(build.externals.iter().map(|e| e.dir.clone()));
    let contexts = Contexts {
        root,
        target_dir,
        externals: build.externals,
        bases: &bases,
    };
    let extra = collect_extra(&contexts, &profile_dirs(artifacts, target_dir));
    let entry = Entry {
        version: ENTRY_VERSION,
        profile: profile.to_string(),
        artifacts: stored,
        extra,
    };
    store.put_entry(NAMESPACE, key, &entry)?;
    statcache.save(target_dir)?;
    store.record("whole-miss", &key.short());
    Ok(())
}

/// The directory cargo put this build in (`<target>/debug`, `<target>/<triple>/release`, ...),
/// derived from the artifacts rather than from the arguments so custom profiles work too.
fn profile_dirs(artifacts: &[PathBuf], target_dir: &Path) -> Vec<PathBuf> {
    let mut out = BTreeSet::new();
    for path in artifacts {
        let Some(parent) = path.parent() else {
            continue;
        };
        let dir = if parent.file_name() == Some(OsStr::new("deps")) {
            parent.parent().unwrap_or(parent)
        } else {
            parent
        };
        if dir.starts_with(target_dir) {
            out.insert(dir.to_path_buf());
        }
    }
    out.into_iter().collect()
}

fn profile_name(args: &[OsString]) -> String {
    if let Some(profile) = inputs::flag_value(args, "--profile") {
        return profile.to_string_lossy().into_owned();
    }
    if args.iter().any(|a| a == "--release") {
        return "release".to_string();
    }
    "dev".to_string()
}

/// Everything the extra-input scan needs in order to decide what is worth recording.
struct Contexts<'a> {
    root: &'a Path,
    target_dir: &'a Path,
    externals: &'a [inputs::External],
    bases: &'a [PathBuf],
}

/// Inputs the workspace walk cannot see: files a compiler or build script read from outside the
/// workspace, and the environment variables a build script asked cargo to watch.
///
/// Every dep-info file and build script output in the profile directory is read, not only the ones
/// this build wrote. Cargo leaves a build script alone when its own fingerprint says nothing
/// changed, and that script's `rerun-if-env-changed` must still invalidate our entry.
fn collect_extra(contexts: &Contexts, profile_dirs: &[PathBuf]) -> Vec<ExtraInput> {
    let mut files: BTreeMap<String, Option<Digest>> = BTreeMap::new();
    let mut envs: BTreeMap<String, Option<String>> = BTreeMap::new();
    for dir in profile_dirs {
        if let Ok(entries) = fs::read_dir(dir.join("deps")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension() != Some(OsStr::new("d")) {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                for prereq in crate::depinfo::parse_prerequisites(&text) {
                    if let Some(path) = recordable(contexts, Path::new(&prereq)) {
                        let digest = Digest::of_file(Path::new(&path)).ok();
                        files.insert(path, digest);
                    }
                }
            }
        }
        if let Ok(entries) = fs::read_dir(dir.join("build")) {
            for entry in entries.flatten() {
                let Ok(text) = fs::read_to_string(entry.path().join("output")) else {
                    continue;
                };
                for line in text.lines() {
                    if let Some(name) = line.strip_prefix("cargo:rerun-if-env-changed=") {
                        let name = name.trim().to_string();
                        envs.insert(name.clone(), std::env::var(&name).ok());
                    } else if let Some(path) = line.strip_prefix("cargo:rerun-if-changed=") {
                        for path in recordable_watched(contexts, path.trim()) {
                            let digest = Digest::of_file(Path::new(&path)).ok();
                            files.insert(path, digest);
                        }
                    }
                }
            }
        }
    }
    let mut out: Vec<ExtraInput> = files
        .into_iter()
        .map(|(path, digest)| ExtraInput::File { path, digest })
        .collect();
    out.extend(envs.into_iter().map(|(name, value)| ExtraInput::Env { name, value }));
    out
}

/// The absolute path of an input worth recording, or `None` when something else already covers it:
/// the workspace walk covers workspace files, the key already covers external path dependencies,
/// `Cargo.lock` pins the registry and git checkouts, and the target directory is rewritten by the
/// build itself.
fn recordable(contexts: &Contexts, path: &Path) -> Option<String> {
    let abs = inputs::absolute_from(contexts.root, path);
    worth_recording(contexts, &abs).then(|| canonical_string(&abs))
}

/// Every external file a `rerun-if-changed` path can point at.
///
/// A build script's relative paths are resolved against its own package root, and the output file
/// does not say which package wrote it, so every package root is tried. A path that resolves
/// nowhere is still recorded for every candidate root: its later appearance must invalidate the
/// entry, and that is what an absent digest means.
fn recordable_watched(contexts: &Contexts, path: &str) -> Vec<String> {
    let path = Path::new(path);
    if path.is_absolute() {
        return recordable(contexts, path).into_iter().collect();
    }
    let mut out: Vec<String> = contexts
        .bases
        .iter()
        .filter_map(|base| recordable_relative(contexts, base, path))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// A relative `rerun-if-changed` path, resolved against one package root.
fn recordable_relative(contexts: &Contexts, base: &Path, path: &Path) -> Option<String> {
    let abs = base.join(path);
    worth_recording(contexts, &abs).then(|| canonical_string(&abs))
}

fn canonical_string(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| inputs::normalize(path))
        .to_string_lossy()
        .into_owned()
}

fn worth_recording(contexts: &Contexts, abs: &Path) -> bool {
    // Canonical paths, because a relative path such as `helper/../../data.txt` starts with the root
    // textually while pointing outside it. Filtering that as a workspace file would leave an
    // edited external input invisible to the key.
    let real = fs::canonicalize(abs).unwrap_or_else(|_| inputs::normalize(abs));
    let root = fs::canonicalize(contexts.root).unwrap_or_else(|_| inputs::normalize(contexts.root));
    if real.starts_with(&root) || real.starts_with(contexts.target_dir) {
        return false;
    }
    if contexts
        .externals
        .iter()
        .any(|external| real.starts_with(&external.dir))
    {
        return false;
    }
    if let Some(home) = inputs::cargo_home() {
        if real.starts_with(home.join("registry")) || real.starts_with(home.join("git")) {
            return false;
        }
    }
    true
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

#[derive(Serialize, Deserialize)]
struct Entry {
    version: u32,
    /// Profile name for the `Finished` line, e.g. `dev` or `release`.
    profile: String,
    artifacts: Vec<Artifact>,
    extra: Vec<ExtraInput>,
}

#[derive(Serialize, Deserialize)]
struct Artifact {
    /// Path relative to the target directory, so a restore works in any checkout.
    path: String,
    digest: Digest,
    executable: bool,
}

#[derive(Serialize, Deserialize)]
enum ExtraInput {
    /// An input file outside the workspace. `None` means the file did not exist when recorded, so
    /// its appearance is a change too.
    File { path: String, digest: Option<Digest> },
    /// `cargo:rerun-if-env-changed`: the value cargo saw, `None` when it was unset.
    Env { name: String, value: Option<String> },
}
