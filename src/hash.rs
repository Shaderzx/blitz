//! Content digests (BLAKE3) and cache-key building.

use std::fmt;
use std::io;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    pub fn of_bytes(data: &[u8]) -> Digest {
        Digest(*blake3::hash(data).as_bytes())
    }

    pub fn of_file(path: &Path) -> io::Result<Digest> {
        let mut h = blake3::Hasher::new();
        h.update_mmap(path)?;
        Ok(Digest(*h.finalize().as_bytes()))
    }

    pub fn hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    pub fn from_hex(s: &str) -> Option<Digest> {
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(Digest(out))
    }

    /// First 12 hex characters, for log lines.
    pub fn short(&self) -> String {
        self.hex()[..12].to_string()
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.short())
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Digest, D::Error> {
        let s = String::deserialize(d)?;
        Digest::from_hex(&s).ok_or_else(|| serde::de::Error::custom("bad digest"))
    }
}

/// Builds a cache key from labelled parts. Every label and value is length-prefixed, so two
/// different part lists can never hash the same bytes.
pub struct KeyBuilder(blake3::Hasher);

impl KeyBuilder {
    pub fn new(domain: &str) -> KeyBuilder {
        let mut k = KeyBuilder(blake3::Hasher::new());
        k.part("domain", domain.as_bytes());
        k
    }

    pub fn part(&mut self, label: &str, data: &[u8]) -> &mut KeyBuilder {
        self.0.update(&(label.len() as u64).to_le_bytes());
        self.0.update(label.as_bytes());
        self.0.update(&(data.len() as u64).to_le_bytes());
        self.0.update(data);
        self
    }

    pub fn digest(&mut self, label: &str, d: &Digest) -> &mut KeyBuilder {
        self.part(label, &d.0)
    }

    pub fn finish(&self) -> Digest {
        Digest(*self.0.finalize().as_bytes())
    }
}
