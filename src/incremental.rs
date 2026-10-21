//! Incremental seeding: hand a fresh checkout the incremental state of an earlier compile.
//!
//! rustc's incremental compilation reuses its own query cache and object files from an earlier
//! compile of the same crate, even after the sources moved on, and checks every reused result
//! itself. blitz stores the newest finalized session of each incremental directory and puts it
//! back before the first compile in a fresh target directory, so an edited crate resumes from the
//! last session instead of compiling from scratch.
//!
//! The lineage key deliberately leaves the sources, the extern digests and `-C metadata` out: those
//! change between commits, and reusing older state across them is exactly the point. rustc discards
//! what does not match. A cache problem here may cost time, never a build.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::hash::{Digest, KeyBuilder};
use crate::store::Store;
use crate::toolchain;
use crate::wrapper::{self, Invocation};

/// How long a lineage's snapshot is left alone. A second build in the same tree would otherwise
/// re-store state the next build is about to replace; CI asks for `always` so every run leaves the
/// newest state for the next pull request.
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(300);

/// Runs an incremental compiler invocation with seeding before it and a snapshot after it.
///
/// `wrapper` is the workspace wrapper cargo put in front of the compiler (`clippy-driver`), or
/// `None` when blitz runs the compiler directly. `args` are the compiler's own arguments.
pub fn run(rustc: &OsStr, wrapper: Option<&OsStr>, args: &[OsString], inv: &Invocation) -> i32 {
    let krate = inv.crate_name.as_deref().unwrap_or("?");
    let plan = Plan::new(rustc, wrapper, inv);
    match &plan {
        Ok(plan) => plan.seed(),
        Err(e) => wrapper::log_decision("noseed", krate, &format!("{e:#}")),
    }
    // Which crate directory held which session before the compile. Afterwards, the one directory
    // whose session changed is the one this invocation wrote; two changing at once cannot be told
    // apart, and the snapshot is skipped rather than guessed.
    let before = plan.as_ref().ok().map(|p| p.session_state());
    let started = SystemTime::now();
    let code = compile(rustc, wrapper, args);
    if code == 0
        && let Ok(plan) = plan
    {
        plan.snapshot(before, started);
    }
    code
}

/// Everything one invocation needs to seed and snapshot its crate.
struct Plan {
    store: Store,
    key: Digest,
    /// The `-C incremental=` directory, shared by every crate of the workspace.
    dir: PathBuf,
    krate: String,
    /// Where the digest memo lives, as in the per-unit path.
    target_root: PathBuf,
}

impl Plan {
    fn new(rustc: &OsStr, wrapper: Option<&OsStr>, inv: &Invocation) -> Result<Plan> {
        let krate = inv.crate_name.clone().context("no crate name")?;
        let dir = inv.incremental_dir.clone().context("no incremental directory")?;
        let out_dir = inv.out_dir.as_deref().context("no --out-dir")?;
        let target_root = wrapper::target_root_of(out_dir).context("no target root")?;
        let store = Store::from_env()?;
        let normalizer = wrapper::build_normalizer(&target_root);
        let cache_root = store.writable_root().map(Path::to_path_buf);

        let mut k = KeyBuilder::new("blitz-incr-v1");
        k.part(
            "rustc",
            toolchain::rustc_identity(Path::new(rustc), cache_root.as_deref())?.as_bytes(),
        );
        if let Some(driver) = wrapper {
            // A chained driver decides what the compile produces; its own bytes and timestamp are
            // the only identity a wrapper binary offers.
            k.digest("driver", &binary_identity(driver)?);
        }
        k.part("crate", krate.as_bytes());
        for t in &inv.crate_types {
            k.part("crate-type", t.as_bytes());
        }
        k.part("test", if inv.test { b"yes" } else { b"no" });
        k.part("target", inv.target.as_deref().unwrap_or("<host>").as_bytes());
        k.part("profile", profile_name(out_dir)?.as_bytes());
        let manifest = std::env::var_os("CARGO_MANIFEST_DIR").context("no CARGO_MANIFEST_DIR")?;
        let manifest = Path::new(&manifest);
        k.part("manifest", normalizer.normalize(&manifest.to_string_lossy()).as_bytes());
        // The normalizer maps every package's manifest directory to the same `$MANIFEST` token, so on
        // its own it cannot tell two packages with the same crate name apart, and a bench or example
        // target of one package would share a lineage with the library. Package name, version and the
        // manifest's path inside the workspace keep them distinct without putting the checkout into
        // the key.
        k.part(
            "pkg-name",
            std::env::var("CARGO_PKG_NAME").unwrap_or_default().as_bytes(),
        );
        k.part(
            "pkg-version",
            std::env::var("CARGO_PKG_VERSION").unwrap_or_default().as_bytes(),
        );
        k.part(
            "manifest-rel",
            workspace_relative(manifest)
                .as_deref()
                .unwrap_or("<outside>")
                .as_bytes(),
        );

        Ok(Plan {
            store,
            key: k.finish(),
            dir,
            krate,
            target_root,
        })
    }

    /// Puts the stored session in place when this crate's incremental directory is not there yet.
    fn seed(&self) {
        if disabled() {
            return;
        }
        match self.try_seed() {
            Ok(Some(files)) => wrapper::log_decision("seed", &self.krate, &files.to_string()),
            Ok(None) => {}
            Err(e) => wrapper::log_decision("noseed", &self.krate, &format!("{e:#}")),
        }
    }

    fn try_seed(&self) -> Result<Option<usize>> {
        let Some(entry) = self.store.get_entry::<IncrEntry>("incr", &self.key) else {
            return Ok(None);
        };
        if !self.subdir_ok(&entry.subdir) {
            return Ok(None);
        }
        if !entry.files.iter().any(|f| f.path.ends_with(".lock")) {
            // rustc will not load a session that has no lock file next to it; an entry without one
            // would only cost a copy.
            return Ok(None);
        }
        let dest = self.dir.join(&entry.subdir);
        if dest.exists() {
            // Local state is newer than any snapshot: rustc already has more than this entry holds.
            return Ok(None);
        }
        fs::create_dir_all(&self.dir)?;
        let tmp = self
            .dir
            .join(format!(".blitz-seed-{}-{}", std::process::id(), unique()));
        if let Err(e) = self.materialize(&entry, &tmp) {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
        match fs::rename(&tmp, &dest) {
            Ok(()) => Ok(Some(entry.files.len())),
            Err(e) => {
                let _ = fs::remove_dir_all(&tmp);
                // Another process seeded the same crate first; its state is as good as ours.
                if dest.exists() { Ok(None) } else { Err(e.into()) }
            }
        }
    }

    fn materialize(&self, entry: &IncrEntry, root: &Path) -> Result<()> {
        for file in &entry.files {
            let rel = safe_rel(&file.path)?;
            let dest = root.join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            self.store.materialize(&file.digest, &dest, file.mode)?;
        }
        Ok(())
    }

    /// Stores the newest finalized session this invocation wrote, plus its lock file.
    fn snapshot(&self, before: Option<Result<Vec<SessionState>>>, started: SystemTime) {
        if disabled() || !self.snapshot_due() {
            return;
        }
        let before = match before {
            Some(Ok(state)) => state,
            Some(Err(e)) => return wrapper::log_decision("nosnap", &self.krate, &format!("before: {e:#}")),
            None => return wrapper::log_decision("nosnap", &self.krate, "no-plan"),
        };
        let began = SystemTime::now();
        match self.try_snapshot(&before, started) {
            Ok(Some((files, bytes))) => {
                let ms = began.elapsed().map(|d| d.as_millis()).unwrap_or(0);
                wrapper::log_decision("snapshot", &self.krate, &format!("files={files} bytes={bytes} ms={ms}"));
            }
            Ok(None) => {}
            Err(e) => wrapper::log_decision("nosnap", &self.krate, &format!("{e:#}")),
        }
    }

    /// True when the stored entry is old enough to be replaced, or CI asked for every run.
    fn snapshot_due(&self) -> bool {
        if std::env::var("BLITZ_INCREMENTAL_SNAPSHOT").is_ok_and(|v| v == "always") {
            return true;
        }
        let Some(entry) = self.store.get_entry::<IncrEntry>("incr", &self.key) else {
            return true;
        };
        if !entry.files.iter().any(|f| f.path.ends_with(".lock")) {
            // An entry from before the lock was recorded cannot seed anything useful.
            return true;
        }
        unix_secs().saturating_sub(entry.stored_at) >= SNAPSHOT_INTERVAL.as_secs()
    }

    fn try_snapshot(&self, before: &[SessionState], started: SystemTime) -> Result<Option<(usize, u64)>> {
        let mut changed: Vec<SessionState> = self
            .session_state()?
            .into_iter()
            .filter(|now| !before.contains(now))
            .collect();
        if changed.is_empty() {
            wrapper::log_decision("nosnap", &self.krate, "no-session");
            return Ok(None);
        }
        if changed.len() > 1 {
            // Two crate directories changed during one compile. Neither can be attributed to this
            // invocation, and picking one would overwrite another crate's lineage with wrong state.
            wrapper::log_decision("nosnap", &self.krate, "ambiguous");
            return Ok(None);
        }
        let state = changed.remove(0);
        let Some((session, mtime)) = state.session else {
            wrapper::log_decision("nosnap", &self.krate, "no-session");
            return Ok(None);
        };
        if mtime < started {
            wrapper::log_decision("nosnap", &self.krate, "session-not-from-this-compile");
            return Ok(None);
        }
        let crate_dir = state.dir;
        let mut files = Vec::new();
        let mut bytes = 0u64;
        self.collect(&crate_dir, &crate_dir.join(&session), &mut files, &mut bytes)?;
        // A finalized session is named `s-<a>-<b>-<hash>`; its lock is `s-<a>-<b>.lock`. rustc does
        // not load a session without it, so an entry that cannot carry one is not worth storing.
        let Some(lock) = lock_file(&crate_dir, &session) else {
            wrapper::log_decision("nosnap", &self.krate, "no-lock");
            return Ok(None);
        };
        if let Some((file, len)) = self.record_file(&crate_dir, &lock)? {
            bytes += len;
            files.push(file);
        } else {
            wrapper::log_decision("nosnap", &self.krate, "lock-unreadable");
            return Ok(None);
        }
        if files.is_empty() {
            wrapper::log_decision("nosnap", &self.krate, "no-files");
            return Ok(None);
        }
        let entry = IncrEntry {
            subdir: crate_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .context("incremental directory has no name")?,
            files,
            stored_at: unix_secs(),
        };
        self.store.put_entry("incr", &self.key, &entry)?;
        Ok(Some((entry.files.len(), bytes)))
    }

    /// Every `<crate>-*` directory of the incremental directory, with the newest finalized session
    /// in each. Taken before and after a compile, the difference names the directory this invocation
    /// wrote.
    fn session_state(&self) -> Result<Vec<SessionState>> {
        let prefix = format!("{}-", self.krate);
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            // A fresh target has no incremental directory until the first compile creates one.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let dir = entry.path();
            if !name.starts_with(&prefix) || !dir.is_dir() {
                continue;
            }
            let session = newest_session(&dir);
            out.push(SessionState { dir, session });
        }
        out.sort_by(|a, b| a.dir.cmp(&b.dir));
        Ok(out)
    }

    fn collect(&self, crate_dir: &Path, dir: &Path, out: &mut Vec<IncrFile>, bytes: &mut u64) -> Result<()> {
        for entry in fs::read_dir(dir)?.flatten() {
            let link = entry.file_type()?.is_symlink();
            let path = entry.path();
            let meta = fs::symlink_metadata(&path)?;
            if link {
                bail!("symlink in an incremental session");
            }
            if meta.is_dir() {
                self.collect(crate_dir, &path, out, bytes)?;
            } else if meta.is_file()
                && let Some((file, len)) = self.record_file(crate_dir, &path)?
            {
                *bytes += len;
                out.push(file);
            }
        }
        Ok(())
    }

    /// Stores one file and returns its entry plus its length.
    fn record_file(&self, crate_dir: &Path, path: &Path) -> Result<Option<(IncrFile, u64)>> {
        let rel = path
            .strip_prefix(crate_dir)
            .with_context(|| format!("{} is outside {}", path.display(), crate_dir.display()))?;
        if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
            bail!("incremental file path is not relative and plain");
        }
        let meta = fs::metadata(path)?;
        // The memo keeps unchanged object files from being hashed again on every snapshot.
        let digest = wrapper::file_digest(&self.target_root, path)?;
        self.store.put_file_with(path, &digest)?;
        Ok(Some((
            IncrFile {
                path: rel.to_string_lossy().into_owned(),
                digest,
                mode: mode_of(&meta),
            },
            meta.len(),
        )))
    }

    /// A stored directory name is one plain path component that names this crate.
    fn subdir_ok(&self, subdir: &str) -> bool {
        let mut parts = Path::new(subdir).components();
        let single = matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none();
        single && subdir.starts_with(&format!("{}-", self.krate))
    }
}

/// One crate directory of the incremental directory and the newest finalized session inside it.
/// Equality covers the session's name and mtime, so a directory a compile wrote into compares
/// different before and after.
#[derive(PartialEq, Eq)]
struct SessionState {
    dir: PathBuf,
    session: Option<(String, SystemTime)>,
}

#[derive(Serialize, Deserialize)]
struct IncrEntry {
    /// `<crate_name>-<id>`, the crate's directory inside the incremental directory.
    subdir: String,
    /// Paths relative to that directory: the session directory's files plus its `.lock`.
    files: Vec<IncrFile>,
    #[serde(default)]
    stored_at: u64,
}

#[derive(Serialize, Deserialize)]
struct IncrFile {
    path: String,
    digest: Digest,
    #[serde(default = "default_mode")]
    mode: u32,
}

fn default_mode() -> u32 {
    0o644
}

/// Identity of a chained compiler driver: its name, size and mtime.
fn binary_identity(driver: &OsStr) -> Result<Digest> {
    let path = Path::new(driver);
    let resolved = if path.components().count() > 1 {
        path.to_path_buf()
    } else {
        wrapper::find_program(path).with_context(|| format!("cannot find {}", path.display()))?
    };
    let meta = fs::metadata(&resolved).with_context(|| format!("stat {}", resolved.display()))?;
    let mut k = KeyBuilder::new("blitz-incr-driver-v1");
    k.part("path", resolved.as_os_str().as_encoded_bytes());
    k.part("len", &meta.len().to_le_bytes());
    k.part("mtime", &mtime_ns(&meta).to_le_bytes());
    Ok(k.finish())
}

/// The profile directory name: `debug` for `target/debug/deps`, and the same `debug` for a build
/// script's `target/debug/build/<pkg>-<hash>`.
fn profile_name(out_dir: &Path) -> Result<String> {
    let name = |p: &Path| p.file_name().map(|f| f.to_string_lossy().into_owned());
    if out_dir.file_name().is_some_and(|f| f == "deps") {
        return out_dir.parent().and_then(name).context("no profile directory");
    }
    if out_dir
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|f| f == "build")
    {
        return out_dir
            .parent()
            .and_then(Path::parent)
            .and_then(name)
            .context("no profile directory");
    }
    name(out_dir).context("no profile directory")
}

/// The manifest path relative to the nearest enclosing workspace root, when there is one. The root
/// is the first ancestor whose `Cargo.toml` declares a `[workspace]` table; a package outside any
/// workspace has none, and `<outside>` is recorded for it instead.
fn workspace_relative(manifest: &Path) -> Option<String> {
    for dir in manifest.ancestors() {
        let Ok(text) = fs::read_to_string(dir.join("Cargo.toml")) else {
            continue;
        };
        if !text.lines().any(|line| line.trim_start().starts_with("[workspace")) {
            continue;
        }
        return manifest
            .strip_prefix(dir)
            .ok()
            .map(|rel| rel.to_string_lossy().into_owned());
    }
    None
}

/// The lock file rustc keeps next to a session: its stem is the session's name without the
/// trailing hash component, so it is a prefix of the session name.
fn lock_file(crate_dir: &Path, session: &str) -> Option<PathBuf> {
    let mut best: Option<(usize, PathBuf)> = None;
    for entry in fs::read_dir(crate_dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".lock") else {
            continue;
        };
        if !session.starts_with(stem) || !entry.path().is_file() {
            continue;
        }
        if best.as_ref().is_none_or(|(len, _)| stem.len() > *len) {
            best = Some((stem.len(), entry.path()));
        }
    }
    best.map(|(_, path)| path)
}

/// The newest finalized session directory inside one crate directory.
fn newest_session(crate_dir: &Path) -> Option<(String, SystemTime)> {
    let mut best: Option<(String, SystemTime)> = None;
    for entry in fs::read_dir(crate_dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("s-") || name.ends_with("-working") {
            continue;
        }
        let meta = fs::metadata(entry.path()).ok()?;
        if !meta.is_dir() {
            continue;
        }
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        if best.as_ref().is_none_or(|(_, t)| mtime > *t) {
            best = Some((name, mtime));
        }
    }
    best
}

fn safe_rel(path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    if p.as_os_str().is_empty() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("stored incremental path {path} is not a plain relative path");
    }
    Ok(p.to_path_buf())
}

fn compile(rustc: &OsStr, driver: Option<&OsStr>, args: &[OsString]) -> i32 {
    match driver {
        Some(driver) => {
            let mut argv = Vec::with_capacity(args.len() + 1);
            argv.push(rustc.to_os_string());
            argv.extend_from_slice(args);
            wrapper::run_program(driver, &argv)
        }
        None => wrapper::run_program(rustc, args),
    }
}

fn disabled() -> bool {
    std::env::var("BLITZ_INCREMENTAL").is_ok_and(|v| v == "off")
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mtime_ns(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
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

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    (nanos << 16) ^ N.fetch_add(1, Ordering::Relaxed)
}
