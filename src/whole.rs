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
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::hash::{Digest, KeyBuilder};
use crate::inputs::{self, StatCache};
use crate::store::Store;
use crate::toolchain;

const NAMESPACE: &str = "whole";
const ENTRY_VERSION: u32 = 3;
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
    // Cargo writes `Cargo.lock` during a build that starts without one, so an entry recorded after
    // such a build could never be looked up again: its key holds a lock file the next build sees.
    let lock_before = root.join("Cargo.lock").is_file();
    // The target directory and, when one sits inside the workspace, every cache store root are
    // written by blitz itself; a walk that hashed them would change the key on every build.
    let mut skips: Vec<PathBuf> = vec![target_dir.clone()];
    skips.extend(store_roots(&store));
    let (key, mut statcache, files, externals) = key_of(&store, &root, &target_dir, args, &skips)?;

    if let Some(entry) = load_hit(&store, &key, &root, &skips) {
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
    run_and_record(&store, args, &build, &key, &mut statcache, lock_before)
}

/// Arguments blitz does not model. Each of them changes what cargo reports or where it writes, so
/// the whole-build cache cannot stand in for the build; plain cargo runs instead.
fn unsupported(args: &[OsString]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let text = args[i].to_string_lossy();
        let (flag, inline) = match text.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (text.as_ref(), None),
        };
        if flag == "--config" {
            // `--config key=value` is fully modelled: the pair is in the key. A config *file* is
            // not: its contents never enter the key, and a `build.target-dir` inside it would move
            // the output blitz records.
            let value = inline
                .map(str::to_string)
                .or_else(|| args.get(i + 1).map(|v| v.to_string_lossy().into_owned()));
            if !value.is_some_and(|v| v.contains('=')) {
                return Some("--config with a file is not modeled; inline key=value is".to_string());
            }
            i += if inline.is_some() { 1 } else { 2 };
            continue;
        }
        let reason = match flag {
            "--message-format" => "--message-format is the caller's to control",
            "--build-plan" => "--build-plan is unstable",
            "--unit-graph" => "--unit-graph is unstable",
            "--out-dir" | "--artifact-dir" => "--out-dir/--artifact-dir write outside the target directory",
            "--timings" => "--timings reports a build blitz would not run",
            _ if flag.starts_with("-Z") => "-Z unstable flags are not modeled",
            _ => {
                i += 1;
                continue;
            }
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
    skips: &[PathBuf],
) -> Result<(Digest, StatCache, Vec<PathBuf>, Vec<inputs::External>)> {
    let cache_root = store.writable_root().map(Path::to_path_buf);
    let mut key = KeyBuilder::new(KEY_DOMAIN);

    let mut files = inputs::walk(root, skips)?;
    // Cargo reads every manifest under the workspace, and an ignore rule can hide one that a
    // `members` glob or a path dependency still reaches. A manifest the walk skipped still changes
    // what cargo builds, so it is keyed like any other input.
    let mut keyed: BTreeSet<PathBuf> = files.iter().cloned().collect();
    for rel in inputs::manifests(root, skips)? {
        if keyed.insert(rel.clone()) {
            files.push(rel);
        }
    }
    files.sort();
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

/// Every cache store root, canonical when it exists. A store can sit inside the workspace (a test
/// run with `BLITZ_CACHE_DIR=./.blitz`, a CI job that mounts it there), and its blobs and entries
/// change on every build, so no walk or watched tree may hash them.
fn store_roots(store: &Store) -> Vec<PathBuf> {
    store
        .roots()
        .map(|(root, _)| fs::canonicalize(root).unwrap_or_else(|_| inputs::normalize(root)))
        .collect()
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

fn load_hit(store: &Store, key: &Digest, root: &Path, skips: &[PathBuf]) -> Option<Entry> {
    let entry: Entry = store.get_entry(NAMESPACE, key)?;
    if entry.version != ENTRY_VERSION || entry.artifacts.is_empty() {
        return None;
    }
    // An entry names the files a restore writes. Anything but a plain relative path could land
    // outside the target directory, so the entry is refused before a single file is written.
    if !entry.artifacts.iter().all(|artifact| safe_relative(&artifact.path)) {
        return None;
    }
    if !store.has_all(entry.artifacts.iter().map(|a| &a.digest)) {
        return None;
    }
    if !extra_inputs_unchanged(&entry.extra, root, skips) {
        return None;
    }
    Some(entry)
}

/// True when every recorded input still holds the value it had when the entry was written.
/// Workspace inputs are resolved against `root`, so another checkout validates its own copy, and a
/// watched tree is rebuilt with the same skips the entry was recorded with.
fn extra_inputs_unchanged(extra: &[ExtraInput], root: &Path, skips: &[PathBuf]) -> bool {
    extra.iter().all(|input| match input {
        ExtraInput::File { path, digest } => file_unchanged(Path::new(path), digest, skips),
        ExtraInput::LocalFile { path, digest } => file_unchanged(&root.join(path), digest, skips),
        ExtraInput::Dir { path, entries } => dir_unchanged(Path::new(path), entries, skips),
        ExtraInput::LocalDir { path, entries } => dir_unchanged(&root.join(path), entries, skips),
        ExtraInput::Env { name, value } => std::env::var(name).ok() == *value,
    })
}

fn file_unchanged(path: &Path, digest: &Option<Digest>, skips: &[PathBuf]) -> bool {
    match inspect(skips, path) {
        Ok(InputValue::Absent) => digest.is_none(),
        Ok(InputValue::File(found)) => digest.as_ref() == Some(&found),
        _ => false,
    }
}

fn dir_unchanged(path: &Path, entries: &[TreeEntry], skips: &[PathBuf]) -> bool {
    matches!(inspect(skips, path), Ok(InputValue::Dir(found)) if found == entries)
}

/// True when `path` is relative and made of plain components only, so joining it onto the target
/// directory cannot escape it.
fn safe_relative(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
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
    lock_before: bool,
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
    if let Err(e) = record(
        store,
        build,
        &profile_name(args),
        key,
        statcache,
        &artifacts,
        lock_before,
    ) {
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
    lock_before: bool,
) -> Result<()> {
    let (root, target_dir, files) = (build.root, build.target_dir, build.files);
    if !lock_before && root.join("Cargo.lock").is_file() {
        // Cargo resolved the dependencies while this build ran, so the entry's key names a lock
        // file that did not exist when the key was computed. Nothing can look that key up again.
        return Ok(());
    }
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
    let keyed: BTreeSet<PathBuf> = files.iter().cloned().collect();
    let mut skips = vec![fs::canonicalize(target_dir).unwrap_or_else(|_| inputs::normalize(target_dir))];
    skips.extend(store_roots(store));
    let contexts = Contexts {
        root,
        externals: build.externals,
        bases: &bases,
        keyed: &keyed,
        root_real: fs::canonicalize(root).unwrap_or_else(|_| inputs::normalize(root)),
        skips,
    };
    let extra = collect_extra(&contexts, &profile_dirs(artifacts, target_dir))?;
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
    if args.iter().any(|a| a == "--release" || a == "-r") {
        return "release".to_string();
    }
    "dev".to_string()
}

/// Everything the extra-input scan needs in order to decide what is worth recording.
struct Contexts<'a> {
    root: &'a Path,
    externals: &'a [inputs::External],
    bases: &'a [PathBuf],
    /// Every file the key already covers, relative to `root`.
    keyed: &'a BTreeSet<PathBuf>,
    root_real: PathBuf,
    /// Directories whose contents blitz writes itself: the target directory and every cache store
    /// root. A watched path inside one of them is never recorded; a watched path that *is* one of
    /// them refuses the entry, because its tree would change with every build.
    skips: Vec<PathBuf>,
}

/// Inputs the workspace walk cannot see: files a compiler or build script read from outside the
/// workspace, and the environment variables a build script asked cargo to watch.
///
/// Every dep-info file and build script output in the profile directory is read, not only the ones
/// this build wrote. Cargo leaves a build script alone when its own fingerprint says nothing
/// changed, and that script's `rerun-if-env-changed` must still invalidate our entry.
fn collect_extra(contexts: &Contexts, profile_dirs: &[PathBuf]) -> Result<Vec<ExtraInput>> {
    let mut files: BTreeMap<(u8, String), Recorded> = BTreeMap::new();
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
                    if let Some(found) = recordable(contexts, Path::new(&prereq))? {
                        files.insert((found.scope.tag(), found.path.clone()), found);
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
                        for found in recordable_watched(contexts, path.trim())? {
                            files.insert((found.scope.tag(), found.path.clone()), found);
                        }
                    }
                }
            }
        }
    }
    let mut out: Vec<ExtraInput> = files.into_values().map(Recorded::into_extra_input).collect();
    out.extend(envs.into_iter().map(|(name, value)| ExtraInput::Env { name, value }));
    Ok(out)
}

/// What a recorded path looked like when the entry was written.
enum InputValue {
    /// The path did not exist. Its later appearance is a change too.
    Absent,
    File(Digest),
    /// A directory watched by `rerun-if-changed`: its whole tree, so an added, removed or edited
    /// file inside it changes the value. A directory has no digest of its own, and a failed hash
    /// must never be stored as "absent": absent means the path is not there.
    Dir(Vec<TreeEntry>),
}

/// A recorded path: where it is stored from, and which value it had.
struct Recorded {
    scope: Scope,
    /// The path as the entry stores it: absolute for `Scope::Absolute`, relative to the workspace
    /// root with `/` separators for `Scope::Workspace`.
    path: String,
    value: InputValue,
}

/// How a recorded path is stored, and how it resolves in another checkout.
#[derive(Clone, Copy)]
enum Scope {
    /// An input outside the workspace: its absolute path is the only name it has.
    Absolute,
    /// An input inside the workspace that the walk did not see. Stored relative to the root, so a
    /// second checkout of the same commit validates its own copy of the file, not the first one's.
    Workspace,
}

impl Scope {
    /// Keeps the two scopes apart in the map the scan builds, without mixing their path strings.
    fn tag(self) -> u8 {
        match self {
            Scope::Absolute => 0,
            Scope::Workspace => 1,
        }
    }
}

impl Recorded {
    fn into_extra_input(self) -> ExtraInput {
        let Recorded { scope, path, value } = self;
        match (scope, value) {
            (Scope::Absolute, InputValue::Absent) => ExtraInput::File { path, digest: None },
            (Scope::Absolute, InputValue::File(digest)) => ExtraInput::File {
                path,
                digest: Some(digest),
            },
            (Scope::Absolute, InputValue::Dir(entries)) => ExtraInput::Dir { path, entries },
            (Scope::Workspace, InputValue::Absent) => ExtraInput::LocalFile { path, digest: None },
            (Scope::Workspace, InputValue::File(digest)) => ExtraInput::LocalFile {
                path,
                digest: Some(digest),
            },
            (Scope::Workspace, InputValue::Dir(entries)) => ExtraInput::LocalDir { path, entries },
        }
    }
}

/// Records `abs`, or `None` when the key already covers it.
///
/// A path that *is* the target directory or a cache store root refuses the whole entry: its tree
/// changes with every build, so recording it could never hit, and dropping it could answer with a
/// stale build. Paths merely inside one of them are not worth recording and are dropped.
fn recordable(contexts: &Contexts, path: &Path) -> Result<Option<Recorded>> {
    let abs = inputs::absolute_from(contexts.root, path);
    // Checked before `scope_of`, which drops everything inside a skip silently: a watch that lands
    // exactly on the target directory or a store root has no stable tree at all.
    if contexts.skips.contains(&real_path(&abs)) {
        bail!(
            "{} is the target directory or a cache store; not storing a whole-build entry",
            abs.display()
        );
    }
    let Some((scope, stored)) = scope_of(contexts, &abs) else {
        return Ok(None);
    };
    let value = inspect(&contexts.skips, &abs)?;
    Ok(Some(Recorded {
        scope,
        path: stored_string(scope, &stored),
        value,
    }))
}

/// Every external file a `rerun-if-changed` path can point at.
///
/// A build script's relative paths are resolved against its own package root, and the output file
/// does not say which package wrote it, so every package root is tried. A path that resolves
/// nowhere is still recorded for every candidate root: its later appearance must invalidate the
/// entry, and that is what an absent value means.
fn recordable_watched(contexts: &Contexts, path: &str) -> Result<Vec<Recorded>> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Ok(recordable(contexts, path)?.into_iter().collect());
    }
    let mut out: Vec<Recorded> = Vec::new();
    for base in contexts.bases {
        if let Some(recorded) = recordable(contexts, &base.join(path))? {
            out.push(recorded);
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    Ok(out)
}

/// The stored form of a path: absolute, or relative to the workspace root.
fn stored_string(scope: Scope, path: &Path) -> String {
    match scope {
        Scope::Absolute => canonical_string(path),
        Scope::Workspace => path.to_string_lossy().replace('\\', "/"),
    }
}

fn canonical_string(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| inputs::normalize(path))
        .to_string_lossy()
        .into_owned()
}

/// What `path` is now: absent, a file's digest, or a directory's sorted tree. Any error other than
/// "does not exist" is returned, and the caller then stores no entry at all: an input it could not
/// read must not be remembered as absent.
fn inspect(skips: &[PathBuf], path: &Path) -> Result<InputValue> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => {
            let digest = Digest::of_file(path).with_context(|| format!("hashing {}", path.display()))?;
            Ok(InputValue::File(digest))
        }
        Ok(meta) if meta.is_dir() => Ok(InputValue::Dir(tree(path, skips)?)),
        Ok(_) => bail!("{} is neither a file nor a directory", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(InputValue::Absent),
        Err(e) => Err(e).with_context(|| format!("statting {}", path.display())),
    }
}

/// Every entry under `dir`, relative to it and sorted: a file with its digest, a directory with
/// none.
///
/// Directories are recorded because a build script can list them: adding, removing or renaming an
/// empty directory changes what `read_dir` returns while every file keeps its digest. The skipped
/// directories are left out, so watching a package root that holds the target directory does not
/// record blitz's own output, and `.git` is left out like the walk leaves it out. Symlinks are
/// followed, and a directory reached twice (a symlink loop) is walked once.
fn tree(dir: &Path, skips: &[PathBuf]) -> Result<Vec<TreeEntry>> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    while let Some(current) = pending.pop() {
        let real = fs::canonicalize(&current).with_context(|| format!("resolving {}", current.display()))?;
        if skips.contains(&real) || !visited.insert(real) {
            continue;
        }
        let mut entries: Vec<fs::DirEntry> = fs::read_dir(&current)
            .with_context(|| format!("reading {}", current.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            if entry.file_name() == OsStr::new(".git") {
                continue;
            }
            let path = entry.path();
            let meta = fs::metadata(&path).with_context(|| format!("statting {}", path.display()))?;
            let rel = path
                .strip_prefix(dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if meta.is_dir() {
                out.push(TreeEntry {
                    path: rel,
                    digest: None,
                });
                pending.push(path);
            } else if meta.is_file() {
                let digest = Digest::of_file(&path).with_context(|| format!("hashing {}", path.display()))?;
                out.push(TreeEntry {
                    path: rel,
                    digest: Some(digest),
                });
            } else {
                bail!("{} is neither a file nor a directory", path.display());
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Where an input lives, if it is worth recording at all: `None` when the key already covers it
/// (the walked workspace files, the externals, the registry and git checkouts) or when blitz itself
/// writes it (the target directory, a cache store root).
fn scope_of(contexts: &Contexts, abs: &Path) -> Option<(Scope, PathBuf)> {
    let real = real_path(abs);
    if contexts.skips.iter().any(|skip| real.starts_with(skip)) {
        return None;
    }
    if let Ok(rel) = real.strip_prefix(&contexts.root_real) {
        // The key already holds every walked workspace file. A file under the root that the walk
        // did not see was hidden by an ignore rule (a gitignored `data/`, a vendored tree), and
        // dropping it would leave an edit to it invisible.
        return (!contexts.keyed.contains(rel)).then(|| (Scope::Workspace, rel.to_path_buf()));
    }
    if contexts
        .externals
        .iter()
        .any(|external| real.starts_with(&external.dir))
    {
        return None;
    }
    if let Some(home) = inputs::cargo_home() {
        if real.starts_with(home.join("registry")) || real.starts_with(home.join("git")) {
            return None;
        }
    }
    Some((Scope::Absolute, real))
}

/// The real path of `path` when it exists, else its lexically cleaned absolute form. Canonical
/// paths are what every comparison here needs: a relative path such as `helper/../../data.txt`
/// starts with the root textually while pointing outside it, and `/tmp` is a symlink on macOS.
fn real_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| inputs::normalize(path))
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
    /// A file outside the workspace, by absolute path. `None` means it did not exist when recorded,
    /// so its appearance is a change too.
    File { path: String, digest: Option<Digest> },
    /// A workspace file the walk did not see, relative to the workspace root.
    LocalFile { path: String, digest: Option<Digest> },
    /// A watched directory outside the workspace, recorded as the sorted tree it held.
    Dir { path: String, entries: Vec<TreeEntry> },
    /// A watched directory inside the workspace, relative to the workspace root.
    LocalDir { path: String, entries: Vec<TreeEntry> },
    /// `cargo:rerun-if-env-changed`: the value cargo saw, `None` when it was unset.
    Env { name: String, value: Option<String> },
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct TreeEntry {
    /// Path inside the watched directory, with `/` separators.
    path: String,
    /// The file's digest, or `None` for a directory. Directories are recorded so that adding,
    /// removing or renaming an empty directory changes the recorded tree.
    digest: Option<Digest>,
}
