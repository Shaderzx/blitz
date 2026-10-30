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

use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::file_metadata::{mode_of, mtime_ns};
use crate::hash::{Digest, KeyBuilder};
use crate::store::Store;
use crate::wrapper;

/// The cache namespace holding one entry per build-script run. Blobs live in the shared CAS, so
/// `blitz gc` trims them with everything else.
const NAMESPACE: &str = "bsrun";
const KEY_DOMAIN: &str = "blitz-bsrun-v3";

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
    if disabled() {
        return wrapper::run_program(real.as_os_str(), rest);
    }
    if !real.is_file() {
        return wrapper::run_program(real.as_os_str(), rest);
    }
    let Ok(ctx) = Ctx::new(real) else {
        // The key cannot be established, so the run cannot be cached: behave as if blitz were absent.
        return wrapper::run_program(real.as_os_str(), rest);
    };
    match ctx.lookup() {
        Ok(Lookup::Hit) => {
            ctx.log("bsrun-hit", "restored");
            ctx.stats("bsrun-hit");
            0
        }
        Ok(Lookup::Miss) => ctx.run(real, rest),
        Ok(Lookup::ReplayError) => 1,
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

/// Leaves a marker at the path the shim passed in `BLITZ_BSRUN_MARK`. The shim falls back to the
/// real script only when blitz exits non-zero and left no marker, which means blitz never started
/// the script: that covers a blitz the shell cannot execute and one the kernel killed before it
/// settled an entry. Any other way of telling those apart would run a script that already ran a
/// second time.
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
    if !Path::new(blitz).is_absolute() || !Path::new(real).is_absolute() {
        bail!("the shim needs absolute paths");
    }
    // The marker sits next to the real binary, in cargo's writable build directory, and carries the
    // shim's pid so a run of the same script never sees another run's marker. The suffix is outside
    // the quotes on purpose: inside them the shell would not expand `$$`.
    let mark = format!("{}.blitz-mark.$$", quote(real)?);
    Ok(format!(
        "#!/bin/sh\n\
         # blitz build-script shim: run this script through blitz's cache, or run it directly when\n\
         # blitz is missing or cannot start. blitz touches $BLITZ_BSRUN_MARK just before it runs the\n\
         # real script, so a non-zero exit with no marker means blitz never started one, and only that\n\
         # falls through. A marker means the script already ran, and its exit code is final: running it\n\
         # again would repeat its side effects and its output.\n\
         if [ -x {blitz} ] && [ \"${{BLITZ_BUILD_SCRIPT_RUN:-on}}\" != off ]; then\n\
         \tBLITZ_BSRUN_MARK={mark}\n\
         \texport BLITZ_BSRUN_MARK\n\
         \trm -f \"$BLITZ_BSRUN_MARK\"\n\
         \t{blitz} build-script-run {real} \"$@\"\n\
         \tcode=$?\n\
         \tif [ -e \"$BLITZ_BSRUN_MARK\" ]; then rm -f \"$BLITZ_BSRUN_MARK\"; exit \"$code\"; fi\n\
         \tif [ \"$code\" -ne 0 ]; then exec {real} \"$@\"; fi\n\
         \texit 0\n\
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
enum Lookup {
    Hit,
    Miss,
    ReplayError,
}

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
        let mut k = KeyBuilder::new(KEY_DOMAIN);
        // The script's own bytes: a recompile, a different compiler or a different linker changes
        // them, and nothing else in the key would notice.
        k.digest("real", &Digest::of_file(real).context("hashing the build script")?);
        // OUT_DIR is the run's output location and a build script often prints it into what cargo
        // parses. CI spells it identically on every run; another path is another key.
        k.part("out-dir", out_dir.as_os_str().as_encoded_bytes());
        for (name, value) in selected_env(env::var("CARGO_PKG_NAME").ok().as_deref())? {
            k.part("env-name", name.as_bytes());
            k.part("env-value", value.as_os_str().as_encoded_bytes());
            if executable_override(&name) {
                let text = value.to_str().context("native tool override is not UTF-8")?;
                if text.is_empty() || text.chars().any(char::is_whitespace) {
                    bail!("native tool override is not a plain executable: {name}");
                }
                let path = wrapper::find_program(Path::new(&value)).context("unresolved native tool override")?;
                let path = fs::canonicalize(path)?;
                k.part("selected-tool-path", path.as_os_str().as_encoded_bytes());
                k.digest("selected-tool", &Digest::of_file(&path)?);
            }
        }
        if let Some(path) = env::var_os("PATH") {
            k.part("path", path.as_encoded_bytes());
        } else {
            k.part("path", b"<unset>");
        }
        let target = env::var_os("TARGET");
        let host = env::var_os("HOST");
        for tool in TOOLS {
            k.part("tool-name", tool.as_bytes());
            k.part(
                "tool",
                &effective_tool_identity(tool, target.as_deref(), host.as_deref()),
            );
        }
        Ok(Ctx {
            store,
            key: k.finish(),
            out_dir,
            manifest,
            label: label_of(real),
        })
    }

    /// Restores `OUT_DIR` and the recorded streams. A replay failure is fatal after stdout starts.
    fn lookup(&self) -> Result<Lookup> {
        let Some(entry) = self.store.get_entry::<BsEntry>(NAMESPACE, &self.key) else {
            self.log("bsrun-miss", "no-entry");
            return Ok(Lookup::Miss);
        };
        if !valid_entry(&entry) {
            self.log("bsrun-miss", "entry-invalid");
            return Ok(Lookup::Miss);
        }
        if !env_matches(&entry.env_deps) {
            self.log("bsrun-miss", "env-changed");
            return Ok(Lookup::Miss);
        }
        if !paths_match(&entry.paths) {
            self.log("bsrun-miss", "path-changed");
            return Ok(Lookup::Miss);
        }
        if !self.blobs_present(&entry) {
            self.log("bsrun-miss", "blobs-missing");
            return Ok(Lookup::Miss);
        }
        if !self.out_dir_empty() {
            // Files already there belong to this run, whatever the key says.
            self.log("bsrun-miss", "out-dir-not-empty");
            return Ok(Lookup::Miss);
        }
        if let Err(e) = self.restore(&entry) {
            let _ = fs::remove_dir_all(&self.out_dir);
            let _ = fs::create_dir_all(&self.out_dir);
            self.log("bsrun-miss", &format!("restore-error: {e:#}"));
            return Ok(Lookup::Miss);
        }
        // Cargo parses these bytes; they are replayed exactly as recorded, on the same streams.
        // A stdout failure may have emitted directives, so never fall back to the real script.
        if let Err(e) = self.replay(&entry) {
            eprintln!("blitz: replaying cached build-script output failed: {e}");
            return Ok(Lookup::ReplayError);
        }
        Ok(Lookup::Hit)
    }

    /// Replays the recorded streams. Cargo parses stdout, so it goes out first and byte for byte;
    /// stderr follows. A failed write is the caller's to empty OUT_DIR over.
    fn replay(&self, entry: &BsEntry) -> std::io::Result<()> {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(entry.stdout.as_bytes())?;
        stdout.flush()?;
        drop(stdout);
        let mut stderr = std::io::stderr().lock();
        stderr.write_all(entry.stderr.as_bytes())?;
        stderr.flush()
    }

    /// A directory has no blob, and a symlink's digest describes the file it resolved to, which is
    /// not restored with it: `restore` reads that file through the link on every lookup. Only a
    /// regular file entry needs its bytes here, or a link that points outside OUT_DIR misses forever.
    fn blobs_present(&self, entry: &BsEntry) -> bool {
        entry
            .files
            .iter()
            .filter(|f| !f.dir && f.link.is_none())
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
        for file in entry.files.iter().filter(|f| f.link.is_none()) {
            let dest = self.out_dir.join(&file.name);
            if file.dir {
                let mut builder = fs::DirBuilder::new();
                builder.recursive(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(&dest)?;
                continue;
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let digest = file.digest.as_ref().context("a file entry has no digest")?;
            self.store.materialize(digest, &dest, file.mode)?;
        }
        for file in entry.files.iter().filter(|f| f.link.is_some()) {
            self.restore_link(file, &self.out_dir.join(&file.name))?;
        }
        // A link may name another link that sorts later. Validate only after every target exists.
        for file in entry.files.iter().filter(|f| f.link.is_some()) {
            if path_digest(&self.out_dir.join(&file.name))? != file.digest {
                bail!("symlink target changed");
            }
        }
        // Populate and check links before restricting access. Children precede parents even when
        // an entry read from a cache is not sorted.
        let mut dirs: Vec<_> = entry.files.iter().filter(|f| f.dir).collect();
        dirs.sort_by_key(|f| std::cmp::Reverse(Path::new(&f.name).components().count()));
        for file in dirs {
            set_mode(&self.out_dir.join(&file.name), file.mode)?;
        }
        Ok(())
    }

    fn restore_link(&self, file: &BsFile, dest: &Path) -> Result<()> {
        let target = file.link.as_deref().context("no link target")?;
        // No writes through links, including filesystem aliases of names in a poisoned entry.
        let parent = dest.parent().context("a symlink has no parent")?;
        let mut dir = self.out_dir.clone();
        for component in parent.strip_prefix(&self.out_dir)?.components() {
            dir.push(component);
            if !fs::symlink_metadata(&dir)?.is_dir() {
                bail!("symlink parent is not a directory");
            }
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, dest)?;
        #[cfg(not(unix))]
        bail!("symlinks in OUT_DIR are not supported on this platform");
        Ok(())
    }

    /// Runs the real script, forwards both streams as they arrive, and stores the run when it
    /// succeeded. The exit code is always the script's.
    fn run(&self, real: &Path, args: &[OsString]) -> i32 {
        let started_empty = self.out_dir_empty();
        let mut child = match Command::new(real)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => {
                mark_started();
                child
            }
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
        let stdout = out_thread
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("stdout pump panicked")));
        let stderr = err_thread
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("stderr pump panicked")));
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
        self.stats("bsrun-miss");
        if code == 0 && started_empty {
            match (stdout, stderr) {
                (Ok(stdout), Ok(stderr)) => {
                    if let Err(e) = self.store_run(&stdout, &stderr) {
                        self.log("bsrun-miss", &format!("store-error: {e:#}"));
                    }
                }
                _ => self.log("bsrun-miss", "stream-error"),
            }
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
        if !valid_entry(&entry) {
            bail!("invalid OUT_DIR entries");
        }
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
                .to_str()
                .context("an OUT_DIR entry name is not UTF-8")?
                .to_owned();
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
                    Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                    Err(e) => return Err(e.into()),
                };
                files.push(BsFile {
                    name,
                    dangling: digest.is_none(),
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
                    dangling: false,
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
                    dangling: false,
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
        let manifest_root = fs::canonicalize(&self.manifest)?;
        let out_root = fs::canonicalize(&self.out_dir)?;
        let mut env_deps = Vec::new();
        let mut paths = Vec::new();
        for line in stdout.lines() {
            let line = line.trim_end_matches('\r');
            if let Some(name) = instruction(line, "rerun-if-env-changed") {
                let value = env::var_os(name).map(|v| v.as_encoded_bytes().to_vec());
                env_deps.push((name.to_string(), value));
            } else if let Some(value) = instruction(line, "rerun-if-changed") {
                let path = Path::new(value);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    self.manifest.join(path)
                };
                let original_inside = !path.components().any(|c| c == Component::ParentDir)
                    && (path.starts_with(&manifest_root) || path.starts_with(&out_root));
                let resolved_inside = fs::canonicalize(&path)
                    .is_ok_and(|resolved| resolved.starts_with(&manifest_root) || resolved.starts_with(&out_root));
                if original_inside && resolved_inside {
                    continue;
                }
                let digest = path_digest(&path)?;
                paths.push(BsPath {
                    path: path.to_str().context("a watched path is not UTF-8")?.to_owned(),
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
    env: Vec<(String, Option<Vec<u8>>)>,
    paths: Vec<BsPath>,
}

#[derive(Serialize, Deserialize)]
struct BsEntry {
    files: Vec<BsFile>,
    /// `cargo:rerun-if-env-changed` names with the value the recorded run saw.
    #[serde(default)]
    env_deps: Vec<(String, Option<Vec<u8>>)>,
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
    /// The file's bytes, or for a symlink the digest of what it resolved to. A link with `None`
    /// records an explicitly dangling target, which must still be absent on lookup. Directories
    /// also have no digest, but cannot carry a link target.
    digest: Option<Digest>,
    #[serde(default = "default_mode")]
    mode: u32,
    #[serde(default)]
    link: Option<String>,
    /// Only a link explicitly recorded as dangling may have no target digest.
    #[serde(default)]
    dangling: bool,
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
///
/// Cargo trims the value, so this does the same. An untrimmed name such as
/// `rerun-if-env-changed=VAR ` names a variable that can never be set: every lookup compares two
/// absences and hits, whatever VAR holds by then. Trimming here rather than at each caller keeps a
/// path with a trailing space from being recorded as one that does not exist.
pub(crate) fn instruction<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let rest = line.trim().strip_prefix("cargo:")?;
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    rest.strip_prefix(name)?.strip_prefix('=').map(str::trim)
}

fn env_path(name: &str) -> Result<PathBuf> {
    let value = env::var_os(name).with_context(|| format!("{name} is not set"))?;
    if value.is_empty() {
        bail!("{name} is empty");
    }
    Ok(PathBuf::from(value))
}

/// The environment cargo set for this run, plus the variables a build script's C tooling reads.
fn selected_env(pkg: Option<&str>) -> Result<Vec<(String, OsString)>> {
    let prefix = pkg.map(|p| p.to_uppercase().replace('-', "_"));
    let mut out = Vec::new();
    for (key, value) in env::vars_os() {
        let Some(name) = key.to_str() else {
            bail!("selected environment name is not UTF-8");
        };
        if !cargo_env(name, prefix.as_deref()) {
            continue;
        }
        out.push((name.to_owned(), value));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn cargo_env(name: &str, prefix: Option<&str>) -> bool {
    if name.starts_with("DEP_") {
        return true;
    }
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
fn effective_tool_identity(tool: &str, target: Option<&OsStr>, host: Option<&OsStr>) -> Vec<u8> {
    let mut out = Vec::new();
    for candidate in tool_candidates(tool, target, host) {
        out.extend_from_slice(candidate.as_os_str().as_encoded_bytes());
        out.push(0);
        out.extend_from_slice(&tool_identity(&candidate));
        out.push(0xff);
    }
    out
}

fn tool_candidates(tool: &str, target: Option<&OsStr>, host: Option<&OsStr>) -> Vec<PathBuf> {
    let mut names = Vec::new();
    for triple in [target, host].into_iter().flatten() {
        let triple = triple.to_string_lossy();
        names.push(PathBuf::from(format!("{triple}-{tool}")));
    }
    names.push(PathBuf::from(tool));
    names
}

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

fn env_matches(recorded: &[(String, Option<Vec<u8>>)]) -> bool {
    recorded.iter().all(|(name, value)| {
        let current = env::var_os(name).map(|v| v.as_encoded_bytes().to_vec());
        current.as_ref() == value.as_ref()
    })
}

fn paths_match(recorded: &[BsPath]) -> bool {
    recorded
        .iter()
        .all(|path| path_digest(Path::new(&path.path)).is_ok_and(|current| current == path.digest))
}

/// A stored name must be a plain relative path: a name that is absolute or carries `..` would be
/// joined onto OUT_DIR and written wherever it points.
fn plain_name(name: &str) -> bool {
    let path = Path::new(name);
    !name.is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn valid_entry(entry: &BsEntry) -> bool {
    let mut kinds = HashMap::new();
    for file in &entry.files {
        if !plain_name(&file.name) || kinds.insert(canonical_name(Path::new(&file.name)), file.dir).is_some() {
            return false;
        }
        if file.dir && (file.link.is_some() || file.digest.is_some())
            || !file.dir && file.link.is_none() && file.digest.is_none()
            || file.dangling != (file.link.is_some() && file.digest.is_none())
        {
            return false;
        }
    }
    for file in &entry.files {
        let mut ancestor = PathBuf::new();
        let components: Vec<_> = Path::new(&file.name).components().collect();
        for component in &components[..components.len().saturating_sub(1)] {
            ancestor.push(component.as_os_str());
            if kinds.get(&canonical_name(&ancestor)).is_some_and(|dir| !dir) {
                return false;
            }
        }
    }
    entry.paths.iter().all(|path| !path.path.is_empty())
}

fn canonical_name(path: &Path) -> PathBuf {
    path.components().collect()
}

fn path_digest(path: &Path) -> Result<Option<Digest>> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Some(Digest::of_file(path)?)),
        Ok(_) => bail!("path is not a regular file: {}", path.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn executable_override(name: &str) -> bool {
    let bare = name
        .strip_prefix("TARGET_")
        .or_else(|| name.strip_prefix("HOST_"))
        .unwrap_or(name);
    ["CC", "CXX", "AR", "PKG_CONFIG", "RUSTC", "RUSTC_LINKER"]
        .iter()
        .any(|base| bare == *base || bare.strip_prefix(base).is_some_and(|rest| rest.starts_with('_')))
        || matches!(name, "CMAKE_C_COMPILER" | "CMAKE_CXX_COMPILER" | "CMAKE_LINKER")
}

fn label_of(real: &Path) -> String {
    let name = real
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "?".to_string());
    name.strip_suffix(".blitz-real").unwrap_or(&name).to_string()
}

fn pump<R: Read>(pipe: Option<R>, to_stderr: bool) -> io::Result<Vec<u8>> {
    let Some(mut pipe) = pipe else {
        return Err(io::Error::other("missing child pipe"));
    };
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut forwarding = Ok(());
    loop {
        let n = match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if forwarding.is_ok() {
            forwarding = if to_stderr {
                io::stderr().write_all(&buf[..n])
            } else {
                io::stdout().write_all(&buf[..n])
            };
        }
        // Keep draining after a forwarding error so the child does not block or receive SIGPIPE.
        out.extend_from_slice(&buf[..n]);
    }
    forwarding?;
    if to_stderr {
        io::stderr().flush()?;
    } else {
        io::stdout().flush()?;
    }
    Ok(out)
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
