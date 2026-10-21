//! Cached build-script runs: the half of a build script cargo never routes through blitz.
//!
//! Cargo compiles a build script through `RUSTC_WRAPPER`, then runs the resulting executable itself,
//! so the per-unit cache answers the compile while every fresh target re-runs the script. On
//! engine's CI build that run is most of the edit build: `aws-lc-sys` spends 11.9 s in its own
//! build script, and nothing of engine starts until it finishes. The wrapper therefore leaves a
//! small `/bin/sh` shim in front of a registry crate's build script; the shim calls
//! `blitz build-script-run <real> "$@"`, which restores `OUT_DIR` and the script's stdout from the
//! cache instead of running it.
//!
//! The key is the script's own bytes, `OUT_DIR` as a literal path, the environment cargo sets for
//! build scripts, the C tooling variables a build script reads, the identity of the compiler tools
//! found on `PATH`, and the crate's own prefixed variables (`AWS_LC_SYS_*`, `RING_*`). Anything the
//! script asked cargo to watch through `cargo:rerun-if-env-changed` and
//! `cargo:rerun-if-changed` is recorded with its value and compared on every lookup. Everything
//! else is a miss: this module may cost a run, never a build, and it must never answer with state
//! the inputs do not describe.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::hash::{Digest, KeyBuilder};
use crate::store::Store;
use crate::wrapper;

/// The cache namespace holding one entry per build-script run. Blobs live in the shared CAS, so
/// `blitz gc` trims them with everything else.
const NAMESPACE: &str = "bsrun";

/// Programs a build script or the C tooling behind it runs. Their identity is part of the key: a new
/// compiler changes the objects the script produces without changing the script.
const TOOLS: &[&str] = &[
    "cc",
    "c++",
    "cmake",
    "ar",
    "ld",
    "perl",
    "pkg-config",
    "make",
    "python3",
    "protoc",
    "nasm",
    "clang",
    "git",
];

/// Environment cargo sets for a build script that describes the build instead of this invocation.
/// `NUM_JOBS` is deliberately absent: it says how much parallelism the machine allows, not what the
/// script produces, and cargo changes it with the job count.
const RUNTIME_ENV: &[&str] = &[
    "TARGET",
    "HOST",
    "PROFILE",
    "OPT_LEVEL",
    "DEBUG",
    "RUSTC",
    "RUSTC_LINKER",
];

/// Environment names the C tooling reads, in their plain form. `<name>_<target>`, `TARGET_<name>`
/// and `HOST_<name>` are matched too.
const C_ENV: &[&str] = &["CC", "CXX", "AR", "CFLAGS", "CXXFLAGS", "LDFLAGS", "CPPFLAGS"];

/// `blitz build-script-run <real> [args...]`: what the shim in front of a build script calls.
pub fn main(args: &[OsString]) -> i32 {
    let Some(first) = args.first() else {
        eprintln!("usage: blitz build-script-run <real build script> [args...]");
        return 2;
    };
    let real = Path::new(first);
    let rest = &args[1..];
    if disabled() || !real.is_file() {
        return run_real(real, rest);
    }
    let Ok(ctx) = Ctx::new(real) else {
        // The key cannot be established, so the run cannot be cached: behave as if blitz were absent.
        return run_real(real, rest);
    };
    match ctx.lookup() {
        Ok(Some(())) => {
            ctx.log("bsrun-hit", "restored");
            ctx.stats("bsrun-hit");
            0
        }
        Ok(None) => ctx.run(real, rest),
        // A cache that cannot be read is not the script's problem; run it and store what it says.
        Err(e) => {
            ctx.log("bsrun-miss", &format!("lookup-error: {e:#}"));
            ctx.run(real, rest)
        }
    }
}

fn disabled() -> bool {
    env::var("BLITZ_BUILD_SCRIPT_RUN").is_ok_and(|v| v == "off") || env::var("BLITZ_DISABLE").is_ok_and(|v| v == "1")
}

/// Runs the real script with nothing cached in front of it, after telling the shim that blitz is the
/// one running it.
fn run_real(real: &Path, args: &[OsString]) -> i32 {
    mark_started();
    wrapper::run_program(real.as_os_str(), args)
}

/// Leaves a marker at the path the shim passed in `BLITZ_BSRUN_MARK`. The shell reports 126 or 127
/// when it cannot execute blitz at all, and so does a script that exits that way itself; the shim
/// falls back to the real script only when no marker exists, because any other way of telling those
/// apart would run a script that already ran a second time.
fn mark_started() {
    let Some(path) = env::var_os("BLITZ_BSRUN_MARK") else {
        return;
    };
    // A marker that cannot be written costs the fallback its precision, never the build.
    let _ = fs::write(PathBuf::from(path), b"");
}

/// Leaves a build script's real executable as `<exe>.blitz-real` and writes the shim in its place.
/// A rename, not a copy: cargo hard-links the executable to `build-script-build` after the compile,
/// so whichever name it runs ends up being the shim.
pub fn install_shim(exe: &Path) -> Result<()> {
    if !exe.is_file() {
        bail!("no build script executable at {}", exe.display());
    }
    let real = real_path(exe);
    fs::rename(exe, &real).with_context(|| format!("moving {} aside", exe.display()))?;
    if let Err(e) = write_shim(exe) {
        // Put the compile's output back: a build that cannot run its build script is worse than one
        // whose run is not cached.
        let _ = fs::rename(&real, exe);
        return Err(e);
    }
    Ok(())
}

/// Where the real build-script executable sits next to its shim.
pub fn real_path(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_os_string();
    name.push(".blitz-real");
    PathBuf::from(name)
}

/// Writes the shim at `exe`, replacing whatever is there. It runs `<exe>.blitz-real` through
/// `blitz build-script-run` while that blitz exists, and runs the real binary directly when it does
/// not: a CI run whose blitz download failed reuses the same target volume, and the build must still
/// work.
pub fn write_shim(exe: &Path) -> Result<()> {
    let real = real_path(exe);
    if !real.is_file() {
        // A shim without the real script next to it can only fail the build; never write one.
        bail!("no real build script at {}", real.display());
    }
    let blitz = env::current_exe().context("locating the blitz binary")?;
    let blitz = blitz.to_str().context("the blitz path is not UTF-8")?;
    let real = real.to_str().context("the build script path is not UTF-8")?;
    let text = shim_text(blitz, real)?;
    let mut tmp = exe.as_os_str().to_os_string();
    tmp.push(".blitz-new");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    set_mode(&tmp, 0o755)?;
    fs::rename(&tmp, exe).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

fn shim_text(blitz: &str, real: &str) -> Result<String> {
    Ok(format!(
        "#!/bin/sh\n\
         # blitz build-script shim: run this script through blitz's cache, or run it directly when\n\
         # blitz is missing or cannot start. blitz touches $BLITZ_BSRUN_MARK just before it runs the\n\
         # real script, so 126 or 127 with no marker is the shell failing to execute blitz itself,\n\
         # and only that falls through. A marker means the script already ran, and its own 126 or 127\n\
         # is final: running it again would repeat its side effects and its output.\n\
         if [ -x {blitz} ] && [ \"${{BLITZ_BUILD_SCRIPT_RUN:-on}}\" != off ]; then\n\
         \tBLITZ_BSRUN_MARK=\"${{TMPDIR:-/tmp}}/blitz-bsrun-$$\"\n\
         \texport BLITZ_BSRUN_MARK\n\
         \trm -f \"$BLITZ_BSRUN_MARK\"\n\
         \t{blitz} build-script-run {real} \"$@\"\n\
         \tcode=$?\n\
         \tif [ \"$code\" -ne 126 ] && [ \"$code\" -ne 127 ]; then exit \"$code\"; fi\n\
         \tif [ -e \"$BLITZ_BSRUN_MARK\" ]; then rm -f \"$BLITZ_BSRUN_MARK\"; exit \"$code\"; fi\n\
         fi\n\
         exec {real} \"$@\"\n",
        blitz = quote(blitz)?,
        real = quote(real)?,
    ))
}

/// Single-quotes a path for `/bin/sh`. A newline cannot be quoted safely on one command line and is
/// refused rather than mangled.
fn quote(path: &str) -> Result<String> {
    if path.contains('\n') {
        bail!("a path with a newline cannot go into the shim");
    }
    Ok(format!("'{}'", path.replace('\'', "'\\''")))
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// Everything one run needs: the key, where its outputs go and how to name it in the log.
struct Ctx {
    store: Store,
    key: Digest,
    out_dir: PathBuf,
    manifest: PathBuf,
    label: String,
}

impl Ctx {
    fn new(real: &Path) -> Result<Ctx> {
        let out_dir = env_path("OUT_DIR")?;
        let manifest = env_path("CARGO_MANIFEST_DIR")?;
        let store = Store::from_env()?;
        let mut k = KeyBuilder::new("blitz-bsrun-v1");
        // The script's own bytes: a recompile, a different compiler or a different linker changes
        // them, and nothing else in the key would notice.
        k.digest("real", &Digest::of_file(real).context("hashing the build script")?);
        // OUT_DIR is the run's output location and a build script often prints it into what cargo
        // parses. CI spells it identically on every run; another path is another key.
        k.part("out-dir", out_dir.as_os_str().as_encoded_bytes());
        for (name, value) in selected_env(env::var("CARGO_PKG_NAME").ok().as_deref()) {
            k.part("env", format!("{name}={value}").as_bytes());
        }
        for tool in TOOLS {
            k.part("tool", &tool_identity(Path::new(tool)));
        }
        Ok(Ctx {
            store,
            key: k.finish(),
            out_dir,
            manifest,
            label: label_of(real),
        })
    }

    /// Restores `OUT_DIR` and the recorded streams. `Some(())` means the run never has to happen;
    /// `None` is an ordinary miss; an error is a miss too, and the caller then runs the script.
    fn lookup(&self) -> Result<Option<()>> {
        let Some(entry) = self.store.get_entry::<BsEntry>(NAMESPACE, &self.key) else {
            self.log("bsrun-miss", "no-entry");
            return Ok(None);
        };
        if !entry.files.iter().all(|f| plain_name(&f.name)) || !entry.paths.iter().all(|p| !p.path.is_empty()) {
            self.log("bsrun-miss", "entry-invalid");
            return Ok(None);
        }
        if !env_matches(&entry.env_deps) {
            self.log("bsrun-miss", "env-changed");
            return Ok(None);
        }
        if !paths_match(&entry.paths) {
            self.log("bsrun-miss", "path-changed");
            return Ok(None);
        }
        if !self.blobs_present(&entry) {
            self.log("bsrun-miss", "blobs-missing");
            return Ok(None);
        }
        if !self.out_dir_empty() {
            // Files already there belong to this run, whatever the key says.
            self.log("bsrun-miss", "out-dir-not-empty");
            return Ok(None);
        }
        if let Err(e) = self.restore(&entry) {
            // Undo a half-written restore so the real script starts from the empty directory cargo
            // gave it.
            let _ = fs::remove_dir_all(&self.out_dir);
            self.log("bsrun-miss", &format!("restore-error: {e:#}"));
            return Ok(None);
        }
        // Cargo parses these bytes; they are replayed exactly as recorded, on the same streams.
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(entry.stdout.as_bytes())?;
        stdout.flush()?;
        drop(stdout);
        let mut stderr = std::io::stderr().lock();
        stderr.write_all(entry.stderr.as_bytes())?;
        stderr.flush()?;
        Ok(Some(()))
    }

    fn blobs_present(&self, entry: &BsEntry) -> bool {
        entry
            .files
            .iter()
            .filter_map(|f| f.digest.as_ref())
            .all(|d| self.store.find_blob(d).is_some())
    }

    fn out_dir_empty(&self) -> bool {
        match fs::read_dir(&self.out_dir) {
            Ok(mut entries) => entries.next().is_none(),
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        }
    }

    fn restore(&self, entry: &BsEntry) -> Result<()> {
        fs::create_dir_all(&self.out_dir)?;
        for file in &entry.files {
            let dest = self.out_dir.join(&file.name);
            if file.dir {
                fs::create_dir_all(&dest)?;
                continue;
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            if file.link.is_some() {
                self.restore_link(file, &dest)?;
            } else {
                let digest = file.digest.as_ref().context("a file entry has no digest")?;
                self.store.materialize(digest, &dest, file.mode)?;
            }
        }
        Ok(())
    }

    fn restore_link(&self, file: &BsFile, dest: &Path) -> Result<()> {
        let target = file.link.as_deref().context("no link target")?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let _ = fs::remove_file(dest);
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, dest)?;
        #[cfg(not(unix))]
        bail!("symlinks in OUT_DIR are not supported on this platform");
        // The link was recorded with the digest of what it resolved to, and that target is not
        // restored with it: a hit is only honest when the target still holds the same bytes.
        if let Some(recorded) = &file.digest
            && Digest::of_file(dest)? != *recorded
        {
            bail!("symlink target changed");
        }
        Ok(())
    }

    /// Runs the real script, forwards both streams as they arrive, and stores the run when it
    /// succeeded. The exit code is always the script's.
    fn run(&self, real: &Path, args: &[OsString]) -> i32 {
        mark_started();
        let mut child = match Command::new(real)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                eprintln!("blitz: cannot run {}: {e}", real.display());
                return 1;
            }
        };
        let out_pipe = child.stdout.take();
        let err_pipe = child.stderr.take();
        let out_thread = std::thread::spawn(move || pump(out_pipe, false));
        let err_thread = std::thread::spawn(move || pump(err_pipe, true));
        let status = child.wait();
        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();
        // The streams were forwarded above; this only decides whether the run is worth storing.
        let code = match status {
            // A script killed by a signal has no code, and a cached miss must still return one: cargo
            // only reads a non-zero status here, and exit 1 is what a shell reports for it.
            Ok(status) => status.code().unwrap_or(1),
            Err(e) => {
                eprintln!("blitz: lost the build script: {e}");
                1
            }
        };
        self.log("bsrun-miss", &format!("ran exit={code}"));
        if code == 0
            && let Err(e) = self.store_run(&stdout, &stderr)
        {
            self.log("bsrun-miss", &format!("store-error: {e:#}"));
        }
        code
    }

    fn store_run(&self, stdout: &[u8], stderr: &[u8]) -> Result<()> {
        let Ok(stdout_text) = std::str::from_utf8(stdout) else {
            bail!("stdout is not UTF-8");
        };
        let Ok(stderr_text) = std::str::from_utf8(stderr) else {
            bail!("stderr is not UTF-8");
        };
        let watched = self.watched(stdout_text)?;
        let mut files = Vec::new();
        let mut bytes = 0u64;
        self.collect(&self.out_dir, &mut files, &mut bytes)?;
        files.sort_by(|a, b| a.name.cmp(&b.name));
        let entry = BsEntry {
            files,
            env_deps: watched.env,
            paths: watched.paths,
            stdout: stdout_text.to_string(),
            stderr: stderr_text.to_string(),
            stored_at: unix_secs(),
        };
        self.store.put_entry(NAMESPACE, &self.key, &entry)?;
        self.log("bsrun-store", &format!("files={} bytes={bytes}", entry.files.len()));
        self.stats("bsrun-store");
        Ok(())
    }

    /// Records OUT_DIR's files, their modes and their links. A symlink to a directory or a special
    /// file is not stored: restoring it would hand the build something the key does not describe.
    fn collect(&self, dir: &Path, files: &mut Vec<BsFile>, bytes: &mut u64) -> Result<()> {
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            let name = path
                .strip_prefix(&self.out_dir)
                .with_context(|| format!("{} is outside OUT_DIR", path.display()))?
                .to_string_lossy()
                .into_owned();
            if !plain_name(&name) {
                bail!("OUT_DIR entry {name} is not a plain relative path");
            }
            let meta = fs::symlink_metadata(&path)?;
            if meta.is_symlink() {
                let target = fs::read_link(&path)?;
                let target = target.to_str().context("a symlink target is not UTF-8")?.to_string();
                let digest = match fs::metadata(&path) {
                    Ok(resolved) if resolved.is_file() => Some(Digest::of_file(&path)?),
                    Ok(resolved) if resolved.is_dir() => bail!("symlink to a directory in OUT_DIR"),
                    Ok(_) => bail!("symlink to a special file in OUT_DIR"),
                    Err(_) => None,
                };
                files.push(BsFile {
                    name,
                    digest,
                    mode: mode_of(&meta),
                    link: Some(target),
                    dir: false,
                });
            } else if meta.is_dir() {
                // An empty directory has no file to carry it, so it needs an entry of its own; without
                // one, a restored OUT_DIR is missing a tree the real run created.
                files.push(BsFile {
                    name,
                    digest: None,
                    mode: mode_of(&meta),
                    link: None,
                    dir: true,
                });
                self.collect(&path, files, bytes)?;
            } else if meta.is_file() {
                *bytes += meta.len();
                let digest = self.store.put_file(&path)?;
                files.push(BsFile {
                    name,
                    digest: Some(digest),
                    mode: mode_of(&meta),
                    link: None,
                    dir: false,
                });
            } else {
                bail!("special file in OUT_DIR");
            }
        }
        Ok(())
    }

    /// The `cargo:rerun-if-*` requests the script printed, with what they say today. A request the
    /// key already covers (anything inside the crate, anything in OUT_DIR) is left out.
    fn watched(&self, stdout: &str) -> Result<Watched> {
        let mut env_deps = Vec::new();
        let mut paths = Vec::new();
        for line in stdout.lines() {
            let line = line.trim_end_matches('\r');
            if let Some(name) = instruction(line, "rerun-if-env-changed") {
                let value = env::var_os(name).map(|v| v.to_string_lossy().into_owned());
                env_deps.push((name.to_string(), value));
            } else if let Some(value) = instruction(line, "rerun-if-changed") {
                let path = Path::new(value);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    self.manifest.join(path)
                };
                if path.starts_with(&self.manifest) || path.starts_with(&self.out_dir) {
                    // Inside the crate: the script's own bytes cover the source tree. Inside OUT_DIR:
                    // the tree is restored with it.
                    continue;
                }
                let digest = match fs::metadata(&path) {
                    Ok(meta) if meta.is_dir() => bail!("rerun-if-changed names the directory {}", path.display()),
                    Ok(meta) if meta.is_file() => Some(Digest::of_file(&path)?),
                    Ok(_) => bail!("rerun-if-changed names the special file {}", path.display()),
                    Err(_) => None,
                };
                paths.push(BsPath {
                    path: path.to_string_lossy().into_owned(),
                    digest,
                });
            }
        }
        env_deps.sort();
        env_deps.dedup();
        paths.sort_by(|a, b| a.path.cmp(&b.path));
        paths.dedup_by(|a, b| a.path == b.path);
        Ok(Watched { env: env_deps, paths })
    }

    /// One line per decision, in the format of the other log lines.
    fn log(&self, kind: &str, reason: &str) {
        wrapper::log_decision(kind, &self.label, reason);
    }

    fn stats(&self, event: &str) {
        self.store.record(event, &self.label);
    }
}

/// What a stored run said it watches: the `cargo:rerun-if-env-changed` names with the value the run
/// saw, and the `cargo:rerun-if-changed` paths outside the crate with the digest of what they held.
struct Watched {
    env: Vec<(String, Option<String>)>,
    paths: Vec<BsPath>,
}

#[derive(Serialize, Deserialize)]
struct BsEntry {
    files: Vec<BsFile>,
    /// `cargo:rerun-if-env-changed` names with the value the recorded run saw.
    #[serde(default)]
    env_deps: Vec<(String, Option<String>)>,
    /// `cargo:rerun-if-changed` paths outside the crate, with the digest of what they held.
    #[serde(default)]
    paths: Vec<BsPath>,
    stdout: String,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    stored_at: u64,
}

#[derive(Serialize, Deserialize)]
struct BsFile {
    /// Path relative to OUT_DIR, with plain components only.
    name: String,
    /// The file's bytes, or for a symlink the digest of what it resolved to. `None` for a dangling
    /// link, which is restored as a link and verifies nothing, and for a directory.
    digest: Option<Digest>,
    #[serde(default = "default_mode")]
    mode: u32,
    #[serde(default)]
    link: Option<String>,
    /// A directory to recreate rather than a file to write. Directories are entries of their own so
    /// that an empty one survives the round trip.
    #[serde(default)]
    dir: bool,
}

#[derive(Serialize, Deserialize)]
struct BsPath {
    path: String,
    /// `None` means the path did not exist when the run was recorded; anything appearing there now
    /// is a miss.
    digest: Option<Digest>,
}

fn default_mode() -> u32 {
    0o644
}

/// The value of a build-script instruction, in either spelling cargo accepts: `cargo:name=value` and,
/// since cargo 1.77, `cargo::name=value`. Cargo treats the two as one instruction, so the cache must
/// too: reading only the old spelling records nothing for a script that prints the new one, and a
/// later lookup then answers with output the recorded inputs do not describe.
pub(crate) fn instruction<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let rest = line.strip_prefix("cargo:")?;
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    rest.strip_prefix(name)?.strip_prefix('=')
}

fn env_path(name: &str) -> Result<PathBuf> {
    let value = env::var_os(name).with_context(|| format!("{name} is not set"))?;
    if value.is_empty() {
        bail!("{name} is empty");
    }
    Ok(PathBuf::from(value))
}

/// The environment cargo set for this run, plus the variables a build script's C tooling reads.
fn selected_env(pkg: Option<&str>) -> Vec<(String, String)> {
    let prefix = pkg.map(|p| p.to_uppercase().replace('-', "_"));
    let mut out = Vec::new();
    for (key, value) in env::vars_os() {
        let name = key.to_string_lossy().into_owned();
        if !cargo_env(&name, prefix.as_deref()) {
            continue;
        }
        out.push((name, value.to_string_lossy().into_owned()));
    }
    out.sort();
    out
}

fn cargo_env(name: &str, prefix: Option<&str>) -> bool {
    if name.starts_with("CARGO_") {
        // The jobserver token differs on every build and describes nothing the script produces.
        return name != "CARGO_MAKEFLAGS";
    }
    if RUNTIME_ENV.contains(&name) {
        return true;
    }
    if name.starts_with("PKG_CONFIG") || name.starts_with("CMAKE") {
        return true;
    }
    let bare = name
        .strip_prefix("TARGET_")
        .or_else(|| name.strip_prefix("HOST_"))
        .unwrap_or(name);
    if C_ENV.contains(&bare) {
        return true;
    }
    if C_ENV
        .iter()
        .any(|base| bare.strip_prefix(base).is_some_and(|rest| rest.starts_with('_')))
    {
        return true;
    }
    // A crate's own knobs: aws-lc-sys reads AWS_LC_SYS_*, ring reads RING_*.
    prefix.is_some_and(|p| name.strip_prefix(p).is_some_and(|rest| rest.starts_with('_')))
}

/// Identity of one program on PATH: the real file it resolves to, its size and its mtime. A program
/// that is not there is recorded as absent, so installing it later changes the key. The path is
/// canonicalised first, so a link that starts pointing at another tool changes the identity even when
/// the link itself, its size and its mtime stay put.
fn tool_identity(name: &Path) -> Vec<u8> {
    let Some(path) = wrapper::find_program(name) else {
        return b"absent".to_vec();
    };
    let path = fs::canonicalize(&path).unwrap_or(path);
    let Ok(meta) = fs::metadata(&path) else {
        return b"absent".to_vec();
    };
    let mut out = path.as_os_str().as_encoded_bytes().to_vec();
    out.extend_from_slice(&meta.len().to_le_bytes());
    out.extend_from_slice(&mtime_ns(&meta).to_le_bytes());
    out
}

fn env_matches(recorded: &[(String, Option<String>)]) -> bool {
    recorded.iter().all(|(name, value)| {
        let current = env::var_os(name).map(|v| v.to_string_lossy().into_owned());
        current.as_ref() == value.as_ref()
    })
}

fn paths_match(recorded: &[BsPath]) -> bool {
    recorded.iter().all(|path| {
        let current = match fs::metadata(&path.path) {
            Ok(meta) if meta.is_file() => Digest::of_file(Path::new(&path.path)).ok(),
            _ => None,
        };
        current == path.digest
    })
}

/// A stored name must be a plain relative path: a name that is absolute or carries `..` would be
/// joined onto OUT_DIR and written wherever it points.
fn plain_name(name: &str) -> bool {
    let path = Path::new(name);
    !name.is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn label_of(real: &Path) -> String {
    let name = real
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "?".to_string());
    name.strip_suffix(".blitz-real").unwrap_or(&name).to_string()
}

fn pump<R: Read>(pipe: Option<R>, to_stderr: bool) -> Vec<u8> {
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if to_stderr {
                    let _ = std::io::stderr().write_all(&buf[..n]);
                } else {
                    let _ = std::io::stdout().write_all(&buf[..n]);
                }
                out.extend_from_slice(&buf[..n]);
            }
            Err(_) => break,
        }
    }
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    out
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode_of(_meta: &fs::Metadata) -> u32 {
    0o644
}

fn mtime_ns(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
