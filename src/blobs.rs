//! Files by their content (the owner, 2026-11-13: builders sync whole files over HTTPS, each
//! checked against its SHA-256): a store keyed by hash, and packs, one zstd-compressed tar of
//! blobs named by their hashes. `flow remote` uploads to the server only what it doesn't have; a
//! builder downloads only what it lacks. Every blob is checked against its hash on arrival, so a
//! store never holds a file under the wrong name.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One file of a working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub sha: String,
    pub exec: bool,
    pub size: u64,
}

/// A working tree at one moment: its files, and the commit it's on.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub commit: String,
    pub files: Vec<Entry>,
}

/// The largest blob taken (a build's working tree holds sources, not builds).
pub const MAX_BLOB: u64 = 512 * 1024 * 1024;
/// The most a pack may unpack to, all blobs together.
pub const MAX_PACK: u64 = 8 * 1024 * 1024 * 1024;

pub fn check_sha(sha: &str) -> anyhow::Result<()> {
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        bail!("{sha:?} isn't a SHA-256");
    }
    Ok(())
}

/// A path inside a working tree: relative, plain, without `..`.
pub fn check_path(p: &str) -> anyhow::Result<()> {
    crate::builds::safe_relative(Path::new(p)).map(|_| ())
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self, sha: &str) -> PathBuf {
        self.dir.join(&sha[..2]).join(&sha[2..])
    }

    pub fn has(&self, sha: &str) -> bool {
        check_sha(sha).is_ok() && self.path(sha).is_file()
    }

    /// Keeps `data` if it's the content of `sha`.
    pub fn put(&self, sha: &str, data: &mut impl Read) -> anyhow::Result<()> {
        check_sha(sha)?;
        let dest = self.path(sha);
        if dest.is_file() {
            std::io::copy(&mut data.take(MAX_BLOB + 1), &mut std::io::sink())?;
            // Used again: it stays (see gc).
            let _ = std::fs::File::options()
                .write(true)
                .open(&dest)
                .and_then(|f| f.set_modified(std::time::SystemTime::now()));
            return Ok(());
        }
        let dir = dest.parent().context("a blob's folder")?;
        std::fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        let mut hasher = Sha256::default();
        let mut buf = vec![0u8; 256 * 1024];
        let mut total = 0u64;
        loop {
            let n = data.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > MAX_BLOB {
                bail!("{sha}: larger than {} MB", MAX_BLOB >> 20);
            }
            hasher.update(&buf[..n]);
            tmp.write_all(&buf[..n])?;
        }
        let got = crate::util::hex(&hasher.finalize());
        if got != sha {
            bail!("a file named {sha} has the content of {got}");
        }
        tmp.persist(&dest).map_err(|e| e.error)?;
        Ok(())
    }

    /// The hashes of `wanted` this store doesn't have.
    pub fn missing<'a>(&self, wanted: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut v: Vec<String> = wanted.into_iter().filter(|s| !self.has(s)).map(str::to_owned).collect();
        v.sort();
        v.dedup();
        v
    }

    /// Writes a pack of these blobs.
    pub fn pack(&self, shas: &[String], out: impl Write) -> anyhow::Result<()> {
        let enc = zstd::stream::write::Encoder::new(out, 3)?.auto_finish();
        let mut tar = tar::Builder::new(enc);
        for sha in shas {
            check_sha(sha)?;
            let path = self.path(sha);
            let mut f = std::fs::File::open(&path).with_context(|| format!("blob {sha} isn't here"))?;
            let mut h = tar::Header::new_gnu();
            h.set_size(f.metadata()?.len());
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, sha, &mut f)?;
        }
        tar.finish()?;
        Ok(())
    }

    /// Takes in a pack, checking every blob against its name. Returns how many arrived.
    pub fn unpack(&self, input: impl Read) -> anyhow::Result<usize> {
        let dec = zstd::stream::read::Decoder::new(input)?;
        let mut tar = tar::Archive::new(dec);
        let mut n = 0;
        let mut total = 0u64;
        for entry in tar.entries()? {
            let mut e = entry?;
            total += e.size();
            if total > MAX_PACK {
                bail!("the pack unpacks to more than {} GB", MAX_PACK >> 30);
            }
            let name = e.path()?.to_string_lossy().to_string();
            self.put(&name, &mut e)?;
            n += 1;
        }
        Ok(n)
    }

    /// Deletes blobs nobody used for `age` (a job's snapshot is synced within minutes; the next
    /// one sends again what's gone). Returns the files and bytes freed.
    pub fn gc(&self, age: std::time::Duration) -> (u64, u64) {
        let cutoff = std::time::SystemTime::now() - age;
        let (mut files, mut bytes) = (0, 0);
        let Ok(top) = std::fs::read_dir(&self.dir) else { return (0, 0) };
        for d in top.flatten() {
            let Ok(inner) = std::fs::read_dir(d.path()) else { continue };
            for f in inner.flatten() {
                let Ok(m) = f.metadata() else { continue };
                if m.is_file() && m.modified().is_ok_and(|t| t < cutoff) && std::fs::remove_file(f.path()).is_ok() {
                    files += 1;
                    bytes += m.len();
                }
            }
        }
        (files, bytes)
    }
}

/// The SHA-256 of a file.
pub fn hash_file(path: &Path) -> anyhow::Result<(String, u64)> {
    let f = std::fs::File::open(path)?;
    Ok(crate::builds::sha256_of(f)?)
}

/// Packs files straight from a working tree (flow's upload), named by their hashes.
pub fn pack_files(root: &Path, entries: &[&Entry], out: impl Write) -> anyhow::Result<()> {
    let enc = zstd::stream::write::Encoder::new(out, 3)?.auto_finish();
    let mut tar = tar::Builder::new(enc);
    let mut done = std::collections::HashSet::new();
    for e in entries {
        if !done.insert(e.sha.clone()) {
            continue;
        }
        let mut f = std::fs::File::open(root.join(&e.path))?;
        let mut h = tar::Header::new_gnu();
        h.set_size(f.metadata()?.len());
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, &e.sha, &mut f)?;
    }
    tar.finish()?;
    Ok(())
}

/// Packs bytes named by their hash (a manifest, a git bundle).
pub fn pack_bytes(items: &[(String, Vec<u8>)], out: impl Write) -> anyhow::Result<()> {
    let enc = zstd::stream::write::Encoder::new(out, 3)?.auto_finish();
    let mut tar = tar::Builder::new(enc);
    for (sha, data) in items {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, sha, data.as_slice())?;
    }
    tar.finish()?;
    Ok(())
}

pub fn sha_of(data: &[u8]) -> String {
    crate::util::hex(&Sha256::digest(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_carry_only_what_matches_its_name() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (sa, sb) = (Store::new(a.path()), Store::new(b.path()));
        let data = b"fn main() {}\n".to_vec();
        let sha = sha_of(&data);
        sa.put(&sha, &mut data.as_slice()).unwrap();
        assert!(sa.put(&sha_of(b"other"), &mut data.as_slice()).is_err(), "wrong content refused");
        assert_eq!(sb.missing([sha.as_str()]), vec![sha.clone()]);
        let mut pack = Vec::new();
        sa.pack(std::slice::from_ref(&sha), &mut pack).unwrap();
        assert_eq!(sb.unpack(pack.as_slice()).unwrap(), 1);
        assert!(sb.missing([sha.as_str()]).is_empty());
        // A pack lying about a name doesn't get in.
        let mut bad = Vec::new();
        pack_bytes(&[(sha_of(b"x"), b"y".to_vec())], &mut bad).unwrap();
        assert!(sb.unpack(bad.as_slice()).is_err());
        assert!(check_sha("../../etc/passwd").is_err());
    }
}
