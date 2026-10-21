//! Per-unit cache: blitz as `RUSTC_WRAPPER`.
//!
//! Cargo runs `blitz <rustc> <args...>` for every compiler invocation. A cacheable invocation is
//! answered from the store when the same inputs were compiled before; everything else, and every
//! internal error, falls back to running the real compiler with the original arguments. A broken
//! cache may cost time, it must never cost a build.
//!
//! The key covers the compiler identity, every argument (with checkout-specific paths replaced by
//! placeholders), the contents of every `--extern` library, the environment cargo set for the unit,
//! and the sources the unit reads. A local or git unit gets a `.d` pre-pass, because that is where
//! rustc records which environment variables the unit read, and the key depends on what it says.
//! An unpacked registry crate skips both the pre-pass and the hashing of its sources: crates.io
//! extractions cannot change, and `Cargo.lock` pins the version. Its environment reads are recorded
//! in the entry from the `.d` the real compile writes and compared on every lookup, as ccache's
//! direct mode does, so a cold registry unit costs one compiler process instead of two.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::depinfo;
use crate::hash::{Digest, KeyBuilder};
use crate::normalize::{self, Normalizer};
use crate::store::Store;
use crate::toolchain;

/// Crate types whose output is a function of the sources, the compiler and, for a proc-macro, the
/// linker. Linked artifacts (`staticlib`, `dylib`, `cdylib`, `bin`) stay out: they carry copies of
/// system libraries and linker output that blitz cannot see.
const CACHEABLE_TYPES: &[&str] = &["lib", "rlib", "proc-macro"];

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
    let name = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let compiler = name == "rustc" || name == "clippy-driver" || name.ends_with("-rustc");
    compiler && (p.components().count() > 1 || std::env::var_os("CARGO_PKG_NAME").is_some())
}

/// The blitz subcommands. While cargo is the parent process, anything else in the first argument
/// position is another compiler wrapper, not a subcommand.
const SUBCOMMANDS: &[&str] = &[
    "build",
    "b",
    "stats",
    "gc",
    "cache-dir",
    "build-script-run",
    "help",
    "-h",
    "--help",
    "-V",
    "--version",
];

/// Cargo runs `$RUSTC_WRAPPER $RUSTC_WORKSPACE_WRAPPER $RUSTC <args...>` when both wrappers are set,
/// so blitz can be handed another wrapper as its first argument. That argument is not a blitz
/// subcommand and must not be read as one: run the wrapper with the rest of the command line, so the
/// chain behaves exactly as it would without blitz in the middle.
pub fn chained(args: &[OsString]) -> Option<i32> {
    let first = args.first()?;
    if first.to_str().is_some_and(|f| SUBCOMMANDS.contains(&f)) {
        return None;
    }
    // Cargo passes the real compiler path directly behind the wrapper. When the wrapper is itself
    // a compiler driver (`clippy-driver`), that second argument is the only thing that tells a
    // chained invocation apart from `blitz <compiler> <args>`.
    if !args.get(1).is_some_and(|a| looks_like_compiler(a)) {
        return None;
    }
    if std::env::var_os("CARGO_PKG_NAME").is_none() && find_program(Path::new(first)).is_none() {
        return None;
    }
    // A chained incremental invocation is seeded and snapshotted like a direct one: cargo put this
    // driver in front of the compiler, and the crate behind it is the same crate either way.
    let real = &args[1];
    let rest = &args[2..];
    if std::env::var("BLITZ_DISABLE").is_ok_and(|v| v == "1") {
        // Disabled means plain cargo, and cargo put this wrapper in the chain itself: run it and
        // nothing of blitz, exactly as the unchained path does.
        return Some(run_program(first, &args[1..]));
    }
    let inv = parse(rest);
    if inv.incremental && inv.incremental_dir.is_some() {
        return Some(crate::incremental::run(real, Some(first), rest, &inv));
    }
    Some(run_program(first, &args[1..]))
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
    run_program(rustc, args)
}

/// Runs `program` with `args`, inheriting stdio, and returns its exit code (1 when it cannot start).
pub(crate) fn run_program(program: &OsStr, args: &[OsString]) -> i32 {
    match Command::new(program).args(args).status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("blitz: cannot run {}: {e}", Path::new(program).display());
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
        if inv.incremental_dir.is_some() {
            // Seeding and snapshotting happen around the compiler, which still runs every time.
            return Ok(Outcome::Done(crate::incremental::run(rustc, None, args, &inv)));
        }
        return Ok(Outcome::Pass("incremental"));
    }
    // A build script is the one `bin` unit whose output is a function of its sources and the
    // linker: cargo names it `build_script_*` and never links anything of the user's into it.
    let build_script = krate.starts_with("build_script_");
    let cacheable = inv
        .crate_types
        .iter()
        .all(|t| CACHEABLE_TYPES.contains(&t.as_str()) || (build_script && t == "bin"));
    if inv.crate_types.is_empty() || !cacheable {
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
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
    let registry = manifest
        .as_deref()
        .zip(cargo_home().as_deref())
        .is_some_and(|(manifest, home)| registry_extraction(manifest, home));
    let ctx = Ctx {
        store: &store,
        normalizer: &normalizer,
        target_root: &target_root,
        out_dir: &out_dir,
        krate: &krate,
        extra_filename: &inv.extra_filename,
        manifest: manifest.as_deref(),
        registry,
        build_script,
    };

    let Some(key) = ctx.key(rustc, args, &inv)? else {
        // The key cannot be established: no manifest, no preparse, an unresolvable linker or an
        // OUT_DIR whose contents cannot be pinned down. Compile without the cache rather than risk
        // a hit that does not describe this unit.
        return Ok(Outcome::Pass("inputs-unknown"));
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
    manifest: Option<&'a Path>,
    /// True for an unpacked registry crate, whose sources and environment reads are keyed without a
    /// pre-pass (see `Ctx::sources`).
    registry: bool,
    /// True for a build script, whose executable is keyed and restored like a proc-macro.
    build_script: bool,
}

impl Ctx<'_> {
    /// The cache key, or `None` when the sources cannot be established (the build then runs
    /// uncached rather than risk a stale hit).
    fn key(&self, rustc: &OsStr, args: &[OsString], inv: &Invocation) -> Result<Option<Digest>> {
        let cache_root = self.store.writable_root().map(Path::to_path_buf);
        let mut k = KeyBuilder::new("blitz-unit-v2");
        k.digest(
            "rustc",
            &toolchain::rustc_digest(Path::new(rustc), cache_root.as_deref())?,
        );
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
        if inv.crate_types.iter().any(|t| t == "proc-macro") || self.build_script {
            // A proc-macro is a linked artifact, and rustc records neither the linker nor the SDK it
            // was given. An unresolvable linker means we cannot tell what produced the bytes. A
            // build script is the same kind of artifact.
            match linker_identity(inv.linker.as_deref()) {
                Some(d) => k.digest("linker", &d),
                None => return Ok(None),
            };
        }
        if let Some(out) = std::env::var_os("OUT_DIR").filter(|v| !v.is_empty())
            && !hash_tree(Path::new(&out), &mut k)?
        {
            return Ok(None);
        }
        Ok(Some(k.finish()))
    }

    /// Keys the sources. `None` means the inputs cannot be established, so the unit is not cacheable
    /// this time.
    ///
    /// A registry extraction stops at the normalized manifest path: the extraction cannot change for
    /// a given path and `Cargo.lock` pins its version, so neither the pre-pass nor the source files
    /// earn their place in the key. What it read from the environment is checked against the entry
    /// instead (see `Ctx::env_deps_match`), and refreshed by every miss. Local and git units keep the
    /// pre-pass: their sources can change, so a changed `# env-dep:` line has to change the key
    /// before anything can be restored.
    fn sources(&self, k: &mut KeyBuilder, rustc: &OsStr, args: &[OsString]) -> Result<Option<()>> {
        let Some(manifest) = self.manifest else {
            return Ok(None);
        };
        // The normalized manifest names the crate for a registry extraction ($CARGO_HOME/registry/...)
        // and is a constant for a local one; either way it is recorded before the sources.
        k.part(
            "manifest",
            self.normalizer.normalize(&manifest.to_string_lossy()).as_bytes(),
        );
        if self.registry {
            k.part("sources", b"registry");
            return Ok(Some(()));
        }
        let dir = self.prepass_dir();
        let info = dep_info(rustc, args, &dir);
        let _ = fs::remove_dir_all(&dir);
        let Some(info) = info else {
            return Ok(None);
        };
        k.part("sources", b"dep-info");
        self.key_prerequisites(k, info.prerequisites)?;
        let mut env_deps = info.env_deps;
        env_deps.sort();
        for (name, value) in env_deps {
            let value = value
                .map(|v| self.normalizer.normalize(&v))
                .unwrap_or_else(|| "<unset>".to_string());
            k.part("env-dep", format!("{name}={value}").as_bytes());
        }
        Ok(Some(()))
    }

    /// Keys every source file the pre-pass reported, by normalized path and digest, once each.
    fn key_prerequisites(&self, k: &mut KeyBuilder, deps: Vec<String>) -> Result<()> {
        let mut seen = BTreeSet::new();
        for dep in deps {
            let path = if Path::new(&dep).is_absolute() {
                PathBuf::from(&dep)
            } else {
                std::env::current_dir()?.join(&dep)
            };
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
        Ok(())
    }

    fn prepass_dir(&self) -> PathBuf {
        self.target_root
            .join("blitz")
            .join("prepass")
            .join(format!("{}-{}", std::process::id(), unique()))
    }

    /// Restores the entry when every blob is present. False means "compile it".
    fn restore_hit(&self, key: &Digest) -> Result<bool> {
        let Some(entry) = self.store.get_entry::<UnitEntry>("unit", key) else {
            return Ok(false);
        };
        if !self.env_deps_match(&entry.env_deps) {
            // The environment this unit read has moved since the compile that filled the entry. The
            // recompile stores the entry again with the values it saw, so the next lookup compares
            // against those.
            return Ok(false);
        }
        let prefixes = self.output_prefixes();
        if !entry.outputs.iter().all(|o| output_name_ok(&o.name, &prefixes)) {
            // An entry is JSON read off the disk, not a promise: a name that is not one plain file
            // this unit writes under `--out-dir` would be written wherever it points. Refuse the hit.
            return Ok(false);
        }
        if !self.store.has_all(entry.outputs.iter().map(|o| &o.digest)) {
            return Ok(false);
        }
        for output in &entry.outputs {
            let dest = self.out_dir.join(&output.name);
            let on_disk = if is_dep_info(&output.name) {
                // The stored `.d` carries placeholders; only the restored copy may name real paths.
                let text = String::from_utf8_lossy(&self.store.read_blob(&output.digest)?).into_owned();
                let bytes = self.normalizer.denormalize(&text).into_bytes();
                write_atomic(&dest, &bytes, output.mode)?;
                // The memo has to describe the bytes on disk, which are not the stored text.
                Digest::of_bytes(&bytes)
            } else if self.build_script && self.registry && output.name == self.executable_name() {
                // Only the executable belongs next to the shim. The entry names it
                // under the plain crate name, the name v0.2 used, but the bytes go next to the shim and
                // the shim itself is written fresh because it names this blitz.
                let exe = self.out_dir.join(self.executable_name());
                let real = crate::buildscript::real_path(&exe);
                self.store.materialize(&output.digest, &real, output.mode)?;
                crate::buildscript::write_shim(&exe)?;
                remember_digest(self.target_root, &real, &output.digest);
                continue;
            } else {
                self.store.materialize(&output.digest, &dest, output.mode)?;
                output.digest
            };
            remember_digest(self.target_root, &dest, &on_disk);
        }
        replay(
            &self.normalizer.denormalize(&entry.stdout),
            &self.normalizer.denormalize(&entry.stderr),
        )?;
        Ok(true)
    }

    /// True when every environment variable the recorded compile read still holds the value it saw.
    ///
    /// A registry unit's key does not carry them: they are compared here, against the values the
    /// entry recorded from the `.d` of the compile that produced it. A variable that is set now and
    /// was unset then, or the other way round, is a difference like any other.
    fn env_deps_match(&self, deps: &[(String, Option<String>)]) -> bool {
        deps.iter().all(|(name, recorded)| {
            let current = std::env::var_os(name).map(|v| self.normalizer.normalize(&v.to_string_lossy()));
            current.as_deref() == recorded.as_deref()
        })
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
            log_decision("miss", self.krate, &format!("compile-failed exit={code}"));
            return Ok(Outcome::Done(code));
        }

        let mut outputs = Vec::new();
        let mut skipped = false;
        let mut env_deps = None;
        // Cargo runs a build script itself, after this wrapper returns, so the compile cache cannot
        // answer the run. Renaming the executable and leaving a shim in its place turns that run into
        // `blitz build-script-run`, which caches OUT_DIR and stdout. Only an unpacked registry crate:
        // its sources cannot change under the key, so the script's own bytes and the environment
        // describe the run. A failure leaves the compile's output untouched and logs why.
        let exe_name = self.executable_name();
        let shimmed = self.build_script && self.registry && {
            match crate::buildscript::install_shim(&self.out_dir.join(&exe_name)) {
                Ok(()) => true,
                Err(e) => {
                    log_decision("bsrun-shim", self.krate, &format!("error: {e:#}"));
                    false
                }
            }
        };
        for (name, stamp) in scan_out_dir(self.out_dir, &prefixes) {
            // The shim carries this blitz's path and is written fresh on every restore, so it is not
            // part of the entry. The real executable next to it is stored under the hashed name cargo
            // gave the script (`build_script_build-<meta>`), the name it had before the shim replaced
            // it and the name v0.2 stored: a v0.2 entry restores here, and this entry restores there.
            let stored_name = match (shimmed, name.strip_suffix(".blitz-real")) {
                (true, Some(stem)) => stem.to_string(),
                (true, None) if name == exe_name => continue,
                _ => name.clone(),
            };
            // rustc rewrites every output it produces, so a candidate whose size and mtime are
            // untouched belongs to an earlier build of this unit and is not part of this entry.
            if before.get(&name) == Some(&stamp) {
                skipped = true;
                continue;
            }
            let path = self.out_dir.join(&name);
            // A build script's executable is linked, so it is stored and restored executable; every
            // other output is a plain file.
            let mode = if self.build_script && stored_name == exe_name {
                0o755
            } else {
                0o644
            };
            // The stored `.d` carries placeholders, but the memo has to describe the bytes that are
            // on disk, or a later lookup reuses a digest for text that was never written.
            let (digest, on_disk) = if is_dep_info(&name) {
                let bytes = fs::read(&path)?;
                // The `# env-dep:` lines of the `.d` this compile wrote are the only record of what
                // the unit read from the environment for a registry extraction, which never runs
                // the pre-pass that reports them for the key.
                env_deps = Some(env_deps_of(self.normalizer, &bytes));
                let text = self.normalizer.normalize(&String::from_utf8_lossy(&bytes));
                (self.store.put_bytes(text.as_bytes())?, Digest::of_bytes(&bytes))
            } else {
                let stored = self.store.put_file(&path)?;
                (stored, stored)
            };
            remember_digest(self.target_root, &path, &on_disk);
            outputs.push(UnitOutput {
                name: stored_name,
                digest,
                mode,
            });
        }
        if skipped {
            // This unit normally writes a file that this compile left untouched: storing the rest
            // would make a later hit restore a unit with a missing output, so nothing is stored.
            log_decision("miss", self.krate, "partial-outputs");
            return Ok(Outcome::Done(code));
        }
        if !outputs.iter().any(|o| !is_dep_info(&o.name)) {
            // An entry with nothing to restore would be a hit that leaves the unit unbuilt.
            log_decision("miss", self.krate, "no-outputs");
            return Ok(Outcome::Done(code));
        }
        let entry = UnitEntry {
            outputs,
            // A local or git unit keys its environment reads, so the entry has nothing to compare;
            // a registry unit compares exactly these values on every lookup.
            env_deps: if self.registry {
                let Some(deps) = env_deps else {
                    // No `.d`, so no record of what the unit read. A hit could then ignore a changed
                    // variable, which is worse than not storing the entry.
                    log_decision("miss", self.krate, "no-dep-info");
                    return Ok(Outcome::Done(code));
                };
                deps
            } else {
                Vec::new()
            },
            stdout: self.normalizer.normalize(&String::from_utf8_lossy(&stdout)),
            stderr: self.normalizer.normalize(&String::from_utf8_lossy(&stderr)),
        };
        self.store.put_entry("unit", key, &entry)?;
        self.store.record("unit-miss", self.krate);
        log_decision("miss", self.krate, "compiled");
        Ok(Outcome::Done(code))
    }

    /// The build script's executable name as rustc writes it: the crate name plus `extra-filename`.
    fn executable_name(&self) -> String {
        format!("{}{}", self.krate.replace('-', "_"), self.extra_filename)
    }

    fn output_prefixes(&self) -> Vec<String> {
        let name = self.krate.replace('-', "_");
        let mut prefixes = vec![
            format!("lib{name}{}.", self.extra_filename),
            format!("{name}{}.", self.extra_filename),
        ];
        if self.build_script {
            // A build script is a bin unit: the executable has no extension and rustc names it with
            // the crate name plus `-C extra-filename`, so the prefix has no trailing dot.
            prefixes.push(format!("{name}{}", self.extra_filename));
        }
        prefixes
    }
}

#[derive(Default)]
pub(crate) struct Invocation {
    pub(crate) crate_name: Option<String>,
    pub(crate) out_dir: Option<PathBuf>,
    pub(crate) extra_filename: String,
    pub(crate) linker: Option<String>,
    pub(crate) emit: Vec<String>,
    pub(crate) crate_types: Vec<String>,
    /// True for both `-C incremental` forms, which keep the unit out of the per-unit cache.
    pub(crate) incremental: bool,
    /// The `-C incremental=<dir>` value, when the build named one.
    pub(crate) incremental_dir: Option<PathBuf>,
    pub(crate) test: bool,
    pub(crate) target: Option<String>,
    pub(crate) externs: Vec<(String, String)>,
    pub(crate) inputs: Vec<String>,
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
        if raw == "--test" {
            inv.test = true;
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
            "--target" => inv.target = Some(value),
            "--extern" => {
                if let Some((name, path)) = value.split_once('=') {
                    inv.externs.push((name.to_string(), path.to_string()));
                }
            }
            "-C" | "--codegen" => {
                if let Some(rest) = value.strip_prefix("extra-filename=") {
                    inv.extra_filename = rest.to_string();
                }
                if let Some(rest) = value.strip_prefix("linker=") {
                    inv.linker = Some(rest.to_string());
                }
                if value == "incremental" {
                    inv.incremental = true;
                } else if let Some(rest) = value.strip_prefix("incremental=") {
                    inv.incremental = true;
                    inv.incremental_dir = Some(PathBuf::from(rest));
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

pub(crate) fn target_root_of(out_dir: &Path) -> Option<PathBuf> {
    if out_dir.file_name().is_some_and(|f| f == "deps") {
        out_dir.parent().and_then(Path::parent).map(Path::to_path_buf)
    } else if out_dir
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|f| f == "build")
    {
        // A build script's `--out-dir` is `<target>/<profile>/build/<pkg>-<hash>`, and its compile
        // line still names `<target>/<profile>/deps`. The target root is the directory that makes
        // those paths normalizable, so `-L` and `--extern` arguments do not keep a checkout path.
        out_dir
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .map(Path::to_path_buf)
    } else {
        Some(out_dir.to_path_buf())
    }
}

fn cargo_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CARGO_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home));
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".cargo"))
}

pub(crate) fn build_normalizer(target_root: &Path) -> Normalizer {
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

/// True when the crate is an unpacked registry crate: it sits under `$CARGO_HOME/registry/src` and
/// carries the `.cargo-ok` marker cargo writes after a successful extraction. Such a directory
/// cannot change for a given path and `Cargo.lock` pins the version, so its sources need no hashing.
///
/// Git checkouts are deliberately excluded: a checkout can be patched, rebuilt or replaced in place,
/// so its sources are hashed like a local crate's. So is any registry directory without `.cargo-ok`,
/// because the marker is the only evidence that the extraction finished.
fn registry_extraction(manifest: &Path, cargo_home: &Path) -> bool {
    if !manifest.join("Cargo.toml").is_file() || !manifest.join(".cargo-ok").is_file() {
        return false;
    }
    let root = cargo_home.join("registry").join("src");
    let root_forms = normalize::variants(&root);
    normalize::variants(manifest)
        .iter()
        .any(|m| root_forms.iter().any(|r| Path::new(m).starts_with(r)))
}

/// Identity of the linker a proc-macro is linked with: the `-C linker=` value when the build gave
/// one, else `cc`, resolved on `PATH`, by name, size and mtime. `None` means the linker could not be
/// resolved and the unit is therefore not cached. `SDKROOT`, `DEVELOPER_DIR` and
/// `MACOSX_DEPLOYMENT_TARGET` are keyed with the rest of the environment. Residual risk: a shim
/// linker that forwards to a real linker cannot be told apart from one that stopped doing so, because
/// only the shim's own bytes and timestamp are visible here.
fn linker_identity(linker: Option<&str>) -> Option<Digest> {
    let name = linker.unwrap_or("cc");
    let path = find_program(Path::new(name))?;
    let meta = fs::metadata(&path).ok()?;
    let mut k = KeyBuilder::new("blitz-linker-v1");
    k.part("name", name.as_bytes());
    k.part("len", &meta.len().to_le_bytes());
    k.part("mtime", &mtime_ns(&meta).to_le_bytes());
    Some(k.finish())
}

pub(crate) fn find_program(name: &Path) -> Option<PathBuf> {
    if name.components().count() > 1 {
        return name.is_file().then(|| name.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
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
    let file = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "d"))?;
    let text = String::from_utf8_lossy(&fs::read(&file).ok()?).into_owned();
    Some(DepInfo {
        prerequisites: depinfo::parse_prerequisites(&text),
        env_deps: depinfo::parse_env_deps(&text),
    })
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

/// The environment variables a `.d` file records the compile reading, sorted by name and with each
/// value normalized, so a value that spells a path the same way this checkout does compares equal.
fn env_deps_of(normalizer: &Normalizer, dep_info: &[u8]) -> Vec<(String, Option<String>)> {
    let mut deps: Vec<(String, Option<String>)> = depinfo::parse_env_deps(&String::from_utf8_lossy(dep_info))
        .into_iter()
        .map(|(name, value)| (name, value.map(|v| normalizer.normalize(&v))))
        .collect();
    deps.sort();
    deps
}

/// The environment cargo set for this unit, plus the few variables that change compiler output.
fn key_env(normalizer: &Normalizer) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (key, value) in std::env::vars_os() {
        let name = key.to_string_lossy().into_owned();
        let interesting = (name.starts_with("CARGO_") && name != "CARGO_MAKEFLAGS" && name != "CARGO_HOME")
            // `DEP_*` carries the `links` metadata a build script published for its dependents.
            || name.starts_with("DEP_")
            || matches!(
                name.as_str(),
                "RUSTC_BOOTSTRAP" | "OUT_DIR" | "SDKROOT" | "DEVELOPER_DIR" | "MACOSX_DEPLOYMENT_TARGET"
            );
        if interesting {
            out.insert(name, normalizer.normalize(&value.to_string_lossy()));
        }
    }
    out
}

/// Hashes every file under `root` by relative path, so build-script output cannot change unnoticed.
/// A symlinked file is followed, and the bytes hashed are the target's, which is what the build read.
/// A symlinked directory, a broken link, a special file or a file that vanishes mid-walk returns
/// `false`: the caller then does not cache the unit, which is cheaper than guessing what a tree of
/// links contains.
fn hash_tree(root: &Path, k: &mut KeyBuilder) -> Result<bool> {
    let mut files = Vec::new();
    if !collect_files(root, root, &mut files)? {
        return Ok(false);
    }
    files.sort();
    for rel in files {
        let path = root.join(&rel);
        k.part("out-dir-path", rel.as_bytes());
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {
                k.digest("out-dir-file", &Digest::of_file(&path)?);
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<bool> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let link = entry.file_type()?.is_symlink();
        // `fs::metadata` follows a link, so this is the type of what the build actually read.
        let Ok(meta) = fs::metadata(&path) else {
            return Ok(false);
        };
        if meta.is_dir() {
            if link || !collect_files(root, &path, out)? {
                return Ok(false);
            }
        } else if meta.is_file() {
            let Ok(rel) = path.strip_prefix(root) else {
                return Ok(false);
            };
            out.push(rel.to_string_lossy().into_owned());
        } else {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Serialize, Deserialize)]
struct UnitEntry {
    outputs: Vec<UnitOutput>,
    /// The environment variables the compile read, with the values it saw, from its `.d` file. Only
    /// a registry unit leaves them out of its key, and only that kind of entry fills this in.
    #[serde(default)]
    env_deps: Vec<(String, Option<String>)>,
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
    #[serde(default)]
    inode: u64,
    digest: Digest,
}

/// How close a file's mtime has to be to now for its memo to be distrusted. A file replaced again
/// inside the same clock tick keeps its size and mtime, so a memo written now can already be stale
/// (the racy-clean rule); such a file is simply hashed again.
const RACY: Duration = Duration::from_secs(2);

/// Digest of `path`, memoized in the target directory by size, mtime and inode, so each rlib is
/// hashed once per build instead of once per dependent crate.
pub(crate) fn file_digest(target_root: &Path, path: &Path) -> Result<Digest> {
    let meta = fs::metadata(path)?;
    let len = meta.len();
    let mtime = mtime_ns(&meta);
    let inode = inode_of(&meta);
    let memo_path = memo_path(target_root, path)?;
    if !racy(&meta)
        && let Ok(bytes) = fs::read(&memo_path)
        && let Ok(memo) = serde_json::from_slice::<Memo>(&bytes)
        && memo.len == len
        && memo.mtime == mtime
        && memo.inode == inode
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
    let memo = Memo {
        path: path.to_string_lossy().into_owned(),
        len: meta.len(),
        mtime: mtime_ns(&meta),
        inode: inode_of(&meta),
        digest: *digest,
    };
    if let Ok(bytes) = serde_json::to_vec(&memo) {
        let _ = write_atomic(&memo_path, &bytes, 0o644);
    }
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
fn inode_of(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn inode_of(_meta: &fs::Metadata) -> u64 {
    0
}

fn memo_path(target_root: &Path, path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?;
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

/// True when a stored output name is one plain file name, inside `--out-dir`, that starts with a
/// prefix this unit writes. Nothing from an entry is ever joined onto a path without this check.
fn output_name_ok(name: &str, prefixes: &[String]) -> bool {
    let mut parts = Path::new(name).components();
    let single = matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none();
    single && prefixes.iter().any(|p| name.starts_with(p.as_str()))
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
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    (nanos << 16) ^ N.fetch_add(1, Ordering::Relaxed)
}

/// One line per decision in `$BLITZ_LOG`: `hit|miss|pass <crate> <reason>`.
pub(crate) fn log_decision(kind: &str, krate: &str, reason: &str) {
    let Some(path) = std::env::var_os("BLITZ_LOG") else {
        return;
    };
    let line = format!("{kind} {krate} {reason}\n");
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}
