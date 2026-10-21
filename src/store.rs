//! The cache store: content-addressed blobs plus small JSON action entries, spread over tiers.
//!
//! Layout of one tier:
//!   <root>/cas/<hh>/<hex>              blob, named by the BLAKE3 digest of its bytes
//!   <root>/ac/<ns>/<hh>/<hex>.json     action entry (whole-build or per-unit), named by its key
//!   <root>/tmp/                        staging area; files are renamed into place, never written in place
//!   <root>/stats.log                   one line per cache event, appended
//!
//! The first tier is `BLITZ_CACHE_DIR` (default `$XDG_CACHE_HOME/blitz` or `~/.cache/blitz`) and is
//! writable unless `BLITZ_READONLY=1`. `BLITZ_CACHE_RO` adds read-only tiers (separated by `:`), looked
//! up after the first one. CI uses this to let pull requests read main's cache without writing to it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::hash::Digest;

pub struct Store {
    tiers: Vec<Tier>,
}

struct Tier {
    root: PathBuf,
    writable: bool,
}

impl Store {
    pub fn from_env() -> Result<Store> {
        let primary = match std::env::var_os("BLITZ_CACHE_DIR") {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => default_cache_dir().context("cannot find a cache directory; set BLITZ_CACHE_DIR")?,
        };
        let readonly = std::env::var("BLITZ_READONLY").is_ok_and(|v| v == "1");
        let mut tiers = vec![Tier {
            root: primary,
            writable: !readonly,
        }];
        if let Some(ro) = std::env::var_os("BLITZ_CACHE_RO") {
            for p in std::env::split_paths(&ro) {
                if !p.as_os_str().is_empty() {
                    tiers.push(Tier {
                        root: p,
                        writable: false,
                    });
                }
            }
        }
        Ok(Store { tiers })
    }

    /// The writable tier, if any.
    pub fn writable_root(&self) -> Option<&Path> {
        self.tiers.iter().find(|t| t.writable).map(|t| t.root.as_path())
    }

    pub fn roots(&self) -> impl Iterator<Item = (&Path, bool)> {
        self.tiers.iter().map(|t| (t.root.as_path(), t.writable))
    }

    /// Hashes `src` and stores a copy as a blob (copy-on-write clone where the filesystem allows).
    /// Returns the digest even when no tier is writable.
    pub fn put_file(&self, src: &Path) -> Result<Digest> {
        let d = Digest::of_file(src).with_context(|| format!("hashing {}", src.display()))?;
        if let Some(root) = self.writable_root() {
            let dest = blob_path(root, &d);
            if !dest.exists() {
                let tmp = staging_path(root)?;
                fs::copy(src, &tmp).with_context(|| format!("copying {} into the cache", src.display()))?;
                publish(&tmp, &dest)?;
            }
        }
        Ok(d)
    }

    /// Stores a copy of `src` as the blob named by `d`, which the caller already computed (and
    /// usually memoized). Never re-hashes, so an unchanged object file costs one copy.
    pub fn put_file_with(&self, src: &Path, d: &Digest) -> Result<()> {
        let Some(root) = self.writable_root() else {
            return Ok(());
        };
        let dest = blob_path(root, d);
        if dest.exists() {
            return Ok(());
        }
        let tmp = staging_path(root)?;
        fs::copy(src, &tmp).with_context(|| format!("copying {} into the cache", src.display()))?;
        publish(&tmp, &dest)
    }

    pub fn put_bytes(&self, data: &[u8]) -> Result<Digest> {
        let d = Digest::of_bytes(data);
        if let Some(root) = self.writable_root() {
            let dest = blob_path(root, &d);
            if !dest.exists() {
                let tmp = staging_path(root)?;
                fs::write(&tmp, data)?;
                publish(&tmp, &dest)?;
            }
        }
        Ok(d)
    }

    pub fn find_blob(&self, d: &Digest) -> Option<PathBuf> {
        self.tiers.iter().map(|t| blob_path(&t.root, d)).find(|p| p.is_file())
    }

    pub fn has_all<'a>(&self, digests: impl IntoIterator<Item = &'a Digest>) -> bool {
        digests.into_iter().all(|d| self.find_blob(d).is_some())
    }

    pub fn read_blob(&self, d: &Digest) -> Result<Vec<u8>> {
        let p = self
            .find_blob(d)
            .with_context(|| format!("blob {} is missing", d.short()))?;
        Ok(fs::read(p)?)
    }

    /// Writes blob `d` to `dest`: copied (cloned on APFS, btrfs, XFS) to a temporary file next to
    /// `dest`, given `mode` and a fresh mtime, then renamed over `dest`. Readers never see a partial file.
    ///
    /// With `BLITZ_VERIFY=1` the copy is re-hashed before it is published: a blob that does not match
    /// its name is deleted from a tier this process may write and reported as an error, which every
    /// caller turns into a miss. Verification is off by default because a blob only ever appears by
    /// atomic rename, and hashing a 100 MB binary costs more than the rest of the hit path.
    pub fn materialize(&self, d: &Digest, dest: &Path, mode: u32) -> Result<()> {
        let src = self
            .find_blob(d)
            .with_context(|| format!("blob {} is missing", d.short()))?;
        let dir = dest.parent().context("destination has no parent directory")?;
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".blitz-{}-{}", std::process::id(), unique()));
        fs::copy(&src, &tmp).with_context(|| format!("restoring {}", dest.display()))?;
        if verify_on() && Digest::of_file(&tmp)? != *d {
            let _ = fs::remove_file(&tmp);
            // A corrupt blob is worse than no blob: drop it where this process may write.
            if self.writable_root().is_some_and(|root| src.starts_with(root)) {
                let _ = fs::remove_file(&src);
            }
            bail!("blob {} does not match its digest", d.short());
        }
        set_mode(&tmp, mode)?;
        fs::File::options()
            .write(true)
            .open(&tmp)?
            .set_modified(SystemTime::now())?;
        fs::rename(&tmp, dest).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        Ok(())
    }

    pub fn get_entry<T: DeserializeOwned>(&self, ns: &str, key: &Digest) -> Option<T> {
        self.tiers.iter().find_map(|t| {
            let bytes = fs::read(entry_path(&t.root, ns, key)).ok()?;
            serde_json::from_slice(&bytes).ok()
        })
    }

    pub fn put_entry<T: Serialize>(&self, ns: &str, key: &Digest, value: &T) -> Result<()> {
        let Some(root) = self.writable_root() else {
            return Ok(());
        };
        let tmp = staging_path(root)?;
        fs::write(&tmp, serde_json::to_vec(value)?)?;
        publish(&tmp, &entry_path(root, ns, key))
    }

    /// Appends one event line to the writable tier's stats log. Never fails the caller.
    pub fn record(&self, event: &str, detail: &str) {
        let Some(root) = self.writable_root() else { return };
        let _ = fs::create_dir_all(root);
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("stats.log"))
        {
            let secs = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = f.write_all(format!("{secs} {event} {detail}\n").as_bytes());
        }
    }
}

/// `BLITZ_VERIFY=1` re-hashes every restored blob. Off by default; see `Store::materialize`.
fn verify_on() -> bool {
    std::env::var("BLITZ_VERIFY").is_ok_and(|v| v == "1")
}

fn default_cache_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME").filter(|x| !x.is_empty()) {
        return Some(PathBuf::from(x).join("blitz"));
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".cache").join("blitz"))
}

fn blob_path(root: &Path, d: &Digest) -> PathBuf {
    let hex = d.hex();
    root.join("cas").join(&hex[..2]).join(hex)
}

fn entry_path(root: &Path, ns: &str, key: &Digest) -> PathBuf {
    let hex = key.hex();
    root.join("ac").join(ns).join(&hex[..2]).join(format!("{hex}.json"))
}

fn staging_path(root: &Path) -> Result<PathBuf> {
    let dir = root.join("tmp");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(format!("{}-{}", std::process::id(), unique())))
}

/// Renames a staged file into its final place. Losing a race to an identical writer is fine.
fn publish(tmp: &Path, dest: &Path) -> Result<()> {
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::rename(tmp, dest).inspect_err(|_| {
        let _ = fs::remove_file(tmp);
    })?;
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

#[cfg(unix)]
fn set_mode(p: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_p: &Path, _mode: u32) -> Result<()> {
    Ok(())
}
