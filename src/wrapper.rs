//! Per-unit cache: blitz as `RUSTC_WRAPPER`.
//!
//! Cargo runs `blitz <rustc> <args...>` for every compiler invocation. A cacheable invocation is
//! answered from the store when the same inputs were compiled before; everything else, and every
//! internal error, falls back to running the real compiler with the original arguments. A broken
//! cache may cost time, it must never cost a build.
//!
//! The key covers the compiler identity, every argument (with checkout-specific paths replaced by
//! placeholders), the contents of every `--extern` library, the environment cargo set for the unit,
//! and the sources the unit reads. Sources come from a `.d` pre-pass for local crates; registry and
//! git sources are immutable for a given path, so those units are keyed by the path alone.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::depinfo;
use crate::hash::{Digest, KeyBuilder};
use crate::normalize::{self, Normalizer};
use crate::store::Store;
use crate::toolchain;

/// Crate types whose output is a function of the sources and the compiler alone.
const CACHEABLE_TYPES: &[&str] = &["lib", "rlib", "staticlib", "proc-macro"];

/// Options that take their value as the following argument.
const VALUE_FLAGS: &[&str] = &[
    "--crate-name",
    "--crate-type",
    "--crate-version",
    "--edition",
    "--emit",
    "--out-dir",
    "-o",
    "-L",
    "-l",
    "--extern",
    "--cfg",
    "--check-cfg",
    "--target",
    "--sysroot",
    "--error-format",
    "--json",
    "--color",
    "--codegen",
    "--print",
    "--explain",
    "--cap-lints",
    "--diagnostic-width",
    "--remap-path-prefix",
    "--extern-private",
    "-C",
    "-Z",
    "-W",
    "-A",
    "-D",
    "-F",
    "--allow",
    "--warn",
    "--deny",
    "--forbid",
    "--feature",
];

/// Short options that accept a glued value, as in `-Cincremental=dir` or `-Dwarnings`.
const GLUED_FLAGS: &[&str] = &["-C", "-Z", "-L", "-l", "-W", "-A", "-D", "-F"];

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
    let krate = parse(args).crate_name.unwrap_or_else(|| "?".to_string());
    match run(rustc, args) {
        Ok(Outcome::Done(code)) => code,
        Ok(Outcome::Pass(reason)) => {
            log_decision("pass", &krate, reason);
            passthrough(rustc, args)
        }
        Err(e) => {
            log_decision("pass", &krate, &format!("error: {e:#}"));
            passthrough(rustc, args)
        }
    }
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

/// A finished decision. `Pass` means the caller still has to run the real compiler.
enum Outcome {
    Done(i32),
    Pass(&'static str),
}

fn run(rustc: &OsStr, args: &[OsString]) -> Result<Outcome> {
    if std::env::var("BLITZ_DISABLE").is_ok_and(|v| v == "1") {
        return Ok(Outcome::Pass("disabled"));
    }
    let inv = parse(args);
    if !plain_rustc(rustc) {
        return Ok(Outcome::Pass("compiler-is-not-rustc"));
    }
    let Some(krate) = inv.crate_name.clone() else {
        return Ok(Outcome::Pass("no-crate-name"));
    };
    if inv.inputs.iter().any(|i| i == "-") {
        return Ok(Outcome::Pass("stdin-input"));
    }
    if inv.incremental {
        return Ok(Outcome::Pass("incremental"));
    }
    if inv.crate_types.is_empty() || !inv.crate_types.iter().all(|t| CACHEABLE_TYPES.contains(&t.as_str())) {
        return Ok(Outcome::Pass("crate-type"));
    }
    if !inv.emit.is_empty() && !inv.emit.iter().any(|e| e == "link" || e == "metadata") {
        return Ok(Outcome::Pass("emit-without-link-or-metadata"));
    }
    let Some(out_dir) = inv.out_dir.clone() else {
        return Ok(Outcome::Pass("no-out-dir"));
    };
    let Some(target_root) = target_root_of(&out_dir) else {
        return Ok(Outcome::Pass("no-target-root"));
    };

    let store = Store::from_env()?;
    let normalizer = build_normalizer(&target_root);
    let ctx = Ctx {
        store: &store,
        normalizer: &normalizer,
        target_root: &target_root,
        out_dir: &out_dir,
        krate: &krate,
        extra_filename: &inv.extra_filename,
    };

    let Some(key) = ctx.key(rustc, args, &inv)? else {
        return Ok(Outcome::Pass("dep-info-failed"));
    };
    if ctx.restore_hit(&key)? {
        store.record("unit-hit", &krate);
        log_decision("hit", &krate, "restored");
        return Ok(Outcome::Done(0));
    }
    ctx.compile(rustc, args, &key)
}

/// Everything the cache path needs about one invocation.
struct Ctx<'a> {
    store: &'a Store,
    normalizer: &'a Normalizer,
    target_root: &'a Path,
    out_dir: &'a Path,
    krate: &'a str,
    extra_filename: &'a str,
}

impl Ctx<'_> {
    /// The cache key, or `None` when the sources cannot be established (the build then runs
    /// uncached rather than risk a stale hit).
    fn key(&self, rustc: &OsStr, args: &[OsString], inv: &Invocation) -> Result<Option<Digest>> {
        let cache_root = self.store.writable_root().map(Path::to_path_buf);
        let mut k = KeyBuilder::new("blitz-unit-v1");
        k.digest("rustc", &toolchain::rustc_digest(Path::new(rustc), cache_root.as_deref())?);
        for arg in args {
            k.part("arg", self.normalizer.normalize(&arg.to_string_lossy()).as_bytes());
        }
        for (name, path) in &inv.externs {
            k.part("extern-name", name.as_bytes());
            match file_digest(self.target_root, Path::new(path)) {
                Ok(d) => {
                    k.digest("extern", &d);
                }
                Err(_) => {
                    k.part("extern-missing", path.as_bytes());
                }
            }
        }
        for (name, value) in key_env(self.normalizer) {
            k.part("env", format!("{name}={value}").as_bytes());
        }
        if self.sources(&mut k, rustc, args)?.is_none() {
            return Ok(None);
        }
        if let Some(out) = std::env::var_os("OUT_DIR").filter(|v| !v.is_empty()) {
            hash_tree(Path::new(&out), &mut k)?;
        }
        Ok(Some(k.finish()))
    }

    /// Keys the sources. `None` means the pre-pass failed, so the unit is not cacheable this time.
    fn sources(&self, k: &mut KeyBuilder, rustc: &OsStr, args: &[OsString]) -> Result<Option<()>> {
        let Some(manifest) = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from) else {
            return Ok(None);
        };
        let Some(cargo_home) = cargo_home() else {
            return Ok(None);
        };
        if immutable_source(&manifest, &cargo_home) {
            // crates.io and git sources cannot change for a given path, and Cargo.lock pins both.
            k.part("sources", b"immutable");
            k.part("manifest", self.normalizer.normalize(&manifest.to_string_lossy()).as_bytes());
            return Ok(Some(()));
        }
        let dir = self.prepass_dir();
        let info = dep_info(rustc, args, &dir);
        let _ = fs::remove_dir_all(&dir);
        let Some(info) = info else {
            return Ok(None);
        };
        k.part("sources", b"dep-info");
        let mut seen = BTreeSet::new();
        for dep in info.prerequisites {
            let path =
                if Path::new(&dep).is_absolute() { PathBuf::from(&dep) } else { std::env::current_dir()?.join(&dep) };
            let key_path = self.normalizer.normalize(&path.to_string_lossy());
            if !seen.insert(key_path.clone()) {
                continue;
            }
            k.part("dep-path", key_path.as_bytes());
            match file_digest(self.target_root, &path) {
                Ok(d) => {
                    k.digest("dep", &d);
                }
                Err(_) => {
                    k.part("dep-missing", b"");
                }
            }
        }
        let mut env_deps = info.env_deps;
        env_deps.sort();
        for (name, value) in env_deps {
            let value = value.map(|v| self.normalizer.normalize(&v)).unwrap_or_else(|| "<unset>".to_string());
            k.part("env-dep", format!("{name}={value}").as_bytes());
        }
        Ok(Some(()))
    }

    fn prepass_dir(&self) -> PathBuf {
        self.target_root.join("blitz").join("prepass").join(format!("{}-{}", std::process::id(), unique()))
    }

    /// Restores the entry when every blob is present. False means "compile it".
    fn restore_hit(&self, key: &Digest) -> Result<bool> {
        let Some(entry) = self.store.get_entry::<UnitEntry>("unit", key) else {
            return Ok(false);
        };
        if !self.store.has_all(entry.outputs.iter().map(|o| &o.digest)) {
            return Ok(false);
        }
        for output in &entry.outputs {
            let dest = self.out_dir.join(&output.name);
            if is_dep_info(&output.name) {
                // The stored `.d` carries placeholders; only the restored copy may name real paths.
                let text = String::from_utf8_lossy(&self.store.read_blob(&output.digest)?).into_owned();
                write_atomic(&dest, self.normalizer.denormalize(&text).as_bytes(), output.mode)?;
            } else {
                self.store.materialize(&output.digest, &dest, output.mode)?;
            }
            remember_digest(self.target_root, &dest, &output.digest);
        }
        replay(&self.normalizer.denormalize(&entry.stdout), &self.normalizer.denormalize(&entry.stderr))?;
        Ok(true)
    }

    fn compile(&self, rustc: &OsStr, args: &[OsString], key: &Digest) -> Result<Outcome> {
        let prefixes = self.output_prefixes();
        let before = scan_out_dir(self.out_dir, &prefixes);
        let mut child = Command::new(rustc)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {}", Path::new(rustc).display()))?;
        let stdout_pipe = child.stdout.take().context("stdout pipe")?;
        let stderr_pipe = child.stderr.take().context("stderr pipe")?;
        // Cargo reads the JSON artifact notifications on stderr while the compiler runs, so both
        // streams are forwarded as they arrive rather than replayed at the end.
        let out_thread = std::thread::spawn(move || pump(stdout_pipe, false));
        let err_thread = std::thread::spawn(move || pump(stderr_pipe, true));
        let status = child.wait()?;
        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();
        let code = status.code().unwrap_or(1);
        if code != 0 {
            log_decision("miss", self.krate, "compile-failed");
            return Ok(Outcome::Done(code));
        }

        let mut outputs = Vec::new();
        for (name, stamp) in scan_out_dir(self.out_dir, &prefixes) {
            // rustc rewrites every output it produces, so a candidate whose size and mtime are
            // untouched belongs to an earlier build of this unit and is not part of this entry.
            if before.get(&name) == Some(&stamp) {
                continue;
            }
            let path = self.out_dir.join(&name);
            let digest = if is_dep_info(&name) {
                let text = String::from_utf8_lossy(&fs::read(&path)?).into_owned();
                self.store.put_bytes(self.normalizer.normalize(&text).as_bytes())?
            } else {
                self.store.put_file(&path)?
            };
            remember_digest(self.target_root, &path, &digest);
            outputs.push(UnitOutput { name, digest, mode: 0o644 });
        }
        if !outputs.iter().any(|o| !is_dep_info(&o.name)) {
            // An entry with nothing to restore would be a hit that leaves the unit unbuilt.
            log_decision("miss", self.krate, "no-outputs");
            return Ok(Outcome::Done(code));
        }
        let entry = UnitEntry {
            outputs,
            stdout: self.normalizer.normalize(&String::from_utf8_lossy(&stdout)),
            stderr: self.normalizer.normalize(&String::from_utf8_lossy(&stderr)),
        };
        self.store.put_entry("unit", key, &entry)?;
        self.store.record("unit-miss", self.krate);
        log_decision("miss", self.krate, "compiled");
        Ok(Outcome::Done(code))
    }

    fn output_prefixes(&self) -> Vec<String> {
        let name = self.krate.replace('-', "_");
        vec![format!("lib{name}{}.", self.extra_filename), format!("{name}{}.", self.extra_filename)]
    }
}

#[derive(Default)]
struct Invocation {
    crate_name: Option<String>,
    out_dir: Option<PathBuf>,
    extra_filename: String,
    emit: Vec<String>,
    crate_types: Vec<String>,
    incremental: bool,
    externs: Vec<(String, String)>,
    inputs: Vec<String>,
}

fn parse(args: &[OsString]) -> Invocation {
    let mut inv = Invocation::default();
    let mut i = 0;
    while i < args.len() {
        let raw = args[i].to_string_lossy().into_owned();
        i += 1;
        if raw == "-" || !raw.starts_with('-') {
            inv.inputs.push(raw);
            continue;
        }
        let (flag, inline) = split_flag(&raw);
        let value = match inline {
            Some(v) => Some(v.to_string()),
            None if VALUE_FLAGS.contains(&flag) => {
                let v = args.get(i).map(|a| a.to_string_lossy().into_owned());
                if v.is_some() {
                    i += 1;
                }
                v
            }
            None => None,
        };
        let Some(value) = value else { continue };
        match flag {
            "--crate-name" => inv.crate_name = Some(value),
            "--crate-type" => inv.crate_types.extend(value.split(',').map(str::to_string)),
            "--out-dir" => inv.out_dir = Some(PathBuf::from(value)),
            "--emit" => inv.emit.extend(value.split(',').map(str::to_string)),
            "--extern" => {
                if let Some((name, path)) = value.split_once('=') {
                    inv.externs.push((name.to_string(), path.to_string()));
                }
            }
            "-C" | "--codegen" => {
                if let Some(rest) = value.strip_prefix("extra-filename=") {
                    inv.extra_filename = rest.to_string();
                }
                if value == "incremental" || value.starts_with("incremental=") {
                    inv.incremental = true;
                }
            }
            _ => {}
        }
    }
    inv
}

/// Splits `--flag=value` and glued short forms such as `-Ckey=value` into flag and value.
fn split_flag(arg: &str) -> (&str, Option<&str>) {
    if let Some((flag, value)) = arg.split_once('=')
        && flag.starts_with("--")
    {
        return (flag, Some(value));
    }
    for flag in GLUED_FLAGS {
        if let Some(rest) = arg.strip_prefix(flag)
            && !rest.is_empty()
        {
            return (flag, Some(rest));
        }
    }
    (arg, None)
}

fn plain_rustc(rustc: &OsStr) -> bool {
    Path::new(rustc).file_stem().is_some_and(|s| s == OsStr::new("rustc"))
}

fn target_root_of(out_dir: &Path) -> Option<PathBuf> {
    if out_dir.file_name().is_some_and(|f| f == "deps") {
        out_dir.parent().and_then(Path::parent).map(Path::to_path_buf)
    } else {
        Some(out_dir.to_path_buf())
    }
}

fn cargo_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CARGO_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home));
    }
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".cargo"))
}

fn build_normalizer(target_root: &Path) -> Normalizer {
    let mut n = Normalizer::new();
    n.add(target_root, normalize::TARGET);
    if let Ok(cwd) = std::env::current_dir() {
        n.add(&cwd, normalize::CWD);
    }
    if let Some(manifest) = std::env::var_os("CARGO_MANIFEST_DIR").filter(|m| !m.is_empty()) {
        n.add(Path::new(&manifest), normalize::MANIFEST);
    }
    if let Some(home) = cargo_home() {
        n.add(&home, normalize::CARGO_HOME);
    }
    n
}

/// True when the crate's sources live in an immutable place: a registry extraction or a git checkout.
fn immutable_source(manifest: &Path, cargo_home: &Path) -> bool {
    let roots = [cargo_home.join("registry").join("src"), cargo_home.join("git").join("checkouts")];
    let forms = normalize::variants(manifest);
    roots.iter().any(|root| {
        let root_forms = normalize::variants(root);
        forms.iter().any(|m| root_forms.iter().any(|r| Path::new(m).starts_with(r)))
    })
}

struct DepInfo {
    prerequisites: Vec<String>,
    env_deps: Vec<(String, Option<String>)>,
}

/// Runs the same compile with `--emit=dep-info` into a scratch directory and reads the `.d` it
/// writes. A failure means the real compile will fail too; the caller then runs it uncached.
fn dep_info(rustc: &OsStr, args: &[OsString], dir: &Path) -> Option<DepInfo> {
    fs::create_dir_all(dir).ok()?;
    let status = Command::new(rustc)
        .args(prepass_args(args, dir))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let file = fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "d"))?;
    let text = String::from_utf8_lossy(&fs::read(&file).ok()?).into_owned();
    Some(DepInfo { prerequisites: depinfo::parse_prerequisites(&text), env_deps: depinfo::parse_env_deps(&text) })
}

/// The original arguments with output selection replaced by a dep-info-only run into `dir`.
fn prepass_args(args: &[OsString], dir: &Path) -> Vec<OsString> {
    let mut out = Vec::with_capacity(args.len() + 3);
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].to_string_lossy().into_owned();
        if arg == "--emit" || arg == "--out-dir" || arg == "-o" {
            i += 2;
            continue;
        }
        if arg.starts_with("--emit=") || arg.starts_with("--out-dir=") || arg.starts_with("-o=") {
            i += 1;
            continue;
        }
        out.push(args[i].clone());
        i += 1;
    }
    out.push(OsString::from("--emit=dep-info"));
    out.push(OsString::from("--out-dir"));
    out.push(dir.as_os_str().to_os_string());
    out
}

/// The environment cargo set for this unit, plus the few variables that change compiler output.
fn key_env(normalizer: &Normalizer) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (key, value) in std::env::vars_os() {
        let name = key.to_string_lossy().into_owned();
        let interesting = (name.starts_with("CARGO_") && name != "CARGO_MAKEFLAGS" && name != "CARGO_HOME")
            || matches!(name.as_str(), "RUSTC_BOOTSTRAP" | "OUT_DIR" | "SDKROOT" | "MACOSX_DEPLOYMENT_TARGET");
        if interesting {
            out.insert(name, normalizer.normalize(&value.to_string_lossy()));
        }
    }
    out
}

/// Hashes every file under `root` by relative path, so build-script output cannot change unnoticed.
fn hash_tree(root: &Path, k: &mut KeyBuilder) -> Result<()> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort();
    for rel in files {
        let path = root.join(&rel);
        k.part("out-dir-path", rel.as_bytes());
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {
                k.digest("out-dir-file", &Digest::of_file(&path)?);
            }
            _ => {
                k.part("out-dir-missing", b"");
            }
        }
    }
    Ok(())
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            collect_files(root, &path, out)?;
        } else if meta.is_file()
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct UnitEntry {
    outputs: Vec<UnitOutput>,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
}

#[derive(Serialize, Deserialize)]
struct UnitOutput {
    name: String,
    digest: Digest,
    #[serde(default = "default_mode")]
    mode: u32,
}

fn default_mode() -> u32 {
    0o644
}

#[derive(Serialize, Deserialize)]
struct Memo {
    path: String,
    len: u64,
    mtime: u64,
    digest: Digest,
}

/// Digest of `path`, memoized in the target directory by size and mtime, so each rlib is hashed
/// once per build instead of once per dependent crate.
fn file_digest(target_root: &Path, path: &Path) -> Result<Digest> {
    let meta = fs::metadata(path)?;
    let len = meta.len();
    let mtime = mtime_ns(&meta);
    let memo_path = memo_path(target_root, path)?;
    if let Ok(bytes) = fs::read(&memo_path)
        && let Ok(memo) = serde_json::from_slice::<Memo>(&bytes)
        && memo.len == len
        && memo.mtime == mtime
        && memo.path == path.to_string_lossy()
    {
        return Ok(memo.digest);
    }
    let digest = Digest::of_file(path)?;
    remember_digest(target_root, path, &digest);
    Ok(digest)
}

fn remember_digest(target_root: &Path, path: &Path, digest: &Digest) {
    let Ok(meta) = fs::metadata(path) else { return };
    let Ok(memo_path) = memo_path(target_root, path) else {
        return;
    };
    let memo =
        Memo { path: path.to_string_lossy().into_owned(), len: meta.len(), mtime: mtime_ns(&meta), digest: *digest };
    if let Ok(bytes) = serde_json::to_vec(&memo) {
        let _ = write_atomic(&memo_path, &bytes, 0o644);
    }
}

fn memo_path(target_root: &Path, path: &Path) -> Result<PathBuf> {
    let name = path.file_name().with_context(|| format!("{} has no file name", path.display()))?;
    Ok(target_root.join("blitz").join("digests").join(name))
}

fn mtime_ns(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn scan_out_dir(out_dir: &Path, prefixes: &[String]) -> HashMap<String, (u64, u64)> {
    let mut stamps = HashMap::new();
    let Ok(entries) = fs::read_dir(out_dir) else {
        return stamps;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !prefixes.iter().any(|p| name.starts_with(p.as_str())) {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && meta.is_file()
        {
            stamps.insert(name, (meta.len(), mtime_ns(&meta)));
        }
    }
    stamps
}

/// Copies a compiler pipe through to our matching stream as it arrives, returning a copy.
fn pump<R: Read>(mut pipe: R, to_stderr: bool) -> Vec<u8> {
    let mut all = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                all.extend_from_slice(chunk);
                let flushed = if to_stderr {
                    let mut out = std::io::stderr().lock();
                    out.write_all(chunk).and_then(|()| out.flush())
                } else {
                    let mut out = std::io::stdout().lock();
                    out.write_all(chunk).and_then(|()| out.flush())
                };
                let _ = flushed;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    all
}

fn replay(stdout: &str, stderr: &str) -> Result<()> {
    if !stdout.is_empty() {
        let mut out = std::io::stdout().lock();
        out.write_all(stdout.as_bytes())?;
        out.flush()?;
    }
    if !stderr.is_empty() {
        let mut out = std::io::stderr().lock();
        out.write_all(stderr.as_bytes())?;
        out.flush()?;
    }
    Ok(())
}

fn is_dep_info(name: &str) -> bool {
    name.ends_with(".d")
}

fn write_atomic(dest: &Path, data: &[u8], mode: u32) -> Result<()> {
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = dest.with_file_name(format!(".blitz-{}-{}", std::process::id(), unique()));
    fs::write(&tmp, data)?;
    set_mode(&tmp, mode)?;
    fs::rename(&tmp, dest).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0);
    (nanos << 16) ^ N.fetch_add(1, Ordering::Relaxed)
}

/// One line per decision in `$BLITZ_LOG`: `hit|miss|pass <crate> <reason>`.
fn log_decision(kind: &str, krate: &str, reason: &str) {
    let Some(path) = std::env::var_os("BLITZ_LOG") else {
        return;
    };
    let line = format!("{kind} {krate} {reason}\n");
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}
