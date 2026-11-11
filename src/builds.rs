//! Test builds for the owner, folded in from viber-run (the owner, 2026-11-07: "integrate
//! viber-run into agent flow and then rewrite the viber-run client CLI to talk to the agent flow
//! server instead"). An agent publishes a build as a tar archive; AgentFlow checks it and keeps
//! the archive as it came; the owner's `agentflow run` downloads, verifies and starts it.
//!
//! Like viber-run: each product's builds are numbered and numbers are never reused; a build never
//! changes once published; its name is `<date>-<time>-<topic>-<commit>`.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Largest build accepted (the archive as uploaded).
pub const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub executable: bool,
}

/// What a build archive holds, checked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub files: Vec<FileEntry>,
    pub total: u64,
}

/// Product names: lowercase letters, digits and dashes, like viber-run's.
pub fn check_product(product: &str) -> anyhow::Result<()> {
    let ok = !product.is_empty()
        && product.len() <= 40
        && !product.starts_with('-')
        && product.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        bail!("{product:?} isn't a product name (a-z, 0-9, -)");
    }
    Ok(())
}

/// A path inside an archive that stays inside its folder: relative, no `..`, no odd components.
fn safe_relative(p: &Path) -> anyhow::Result<String> {
    let mut parts = Vec::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str().context("file names must be UTF-8")?.to_owned()),
            Component::CurDir => {}
            _ => bail!("{} leaves the build's folder", p.display()),
        }
    }
    if parts.is_empty() {
        bail!("empty path in the archive");
    }
    Ok(parts.join("/"))
}

/// Reads a tar archive and lists its regular files with checksums. Refuses anything that could
/// write outside the folder it's unpacked into: absolute paths, `..`, links, devices.
pub fn inspect(archive: impl Read) -> anyhow::Result<Manifest> {
    let mut ar = tar::Archive::new(archive);
    let mut files = Vec::new();
    let mut total = 0u64;
    for entry in ar.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type() == tar::EntryType::Directory {
            // "." (a packed folder's top) is fine; anything leaving the folder isn't.
            if entry.path()?.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
                bail!("{} leaves the build's folder", entry.path()?.display());
            }
            continue;
        }
        let path = safe_relative(&entry.path()?)?;
        match entry.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Continuous => {}
            other => bail!("{path}: only files and folders are allowed in a build, not {other:?}"),
        }
        let executable = entry.header().mode().map(|m| m & 0o111 != 0).unwrap_or(false);
        let (sha256, size) = sha256_of(&mut entry)?;
        total += size;
        if total > MAX_BYTES {
            bail!("the build is larger than 2 GB");
        }
        files.push(FileEntry { path, size, sha256, executable });
    }
    if files.is_empty() {
        bail!("the archive holds no files");
    }
    Ok(Manifest { files, total })
}

/// Lets `std::io::copy` feed a hasher (sha2 0.11 hashers aren't `Write`).
struct HashWriter(Sha256);

impl std::io::Write for HashWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// SHA-256 of everything `r` yields, and how many bytes that was.
pub fn sha256_of(mut r: impl Read) -> std::io::Result<(String, u64)> {
    let mut w = HashWriter(Sha256::new());
    let n = std::io::copy(&mut r, &mut w)?;
    Ok((hex(&w.0.finalize()), n))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `2026-11-11-0215-shortcuts-a1b2c3d`, or just the date and time without a commit.
pub fn build_name(stamp: time::OffsetDateTime, branch: &str, commit: &str) -> String {
    let f = time::macros::format_description!("[year]-[month]-[day]-[hour][minute]");
    let date = stamp.format(&f).unwrap_or_default();
    if commit.is_empty() {
        return date;
    }
    let topic: String = branch
        .rsplit('/')
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '-' })
        .collect();
    let topic = if topic.is_empty() { "-".to_owned() } else { topic };
    format!("{date}-{topic}-{commit}")
}

/// Where a build's archive lives on the server.
pub fn archive_path(builds_dir: &Path, product: &str, name: &str) -> PathBuf {
    builds_dir.join(product).join(format!("{name}.tar"))
}

/// Packs a file or folder into a tar archive (the CLI's side of publishing). A single file is
/// stored under its own name; a folder's contents at the archive's top level.
pub fn pack(source: &Path, out: impl std::io::Write) -> anyhow::Result<()> {
    let mut b = tar::Builder::new(out);
    b.follow_symlinks(true);
    if source.is_dir() {
        b.append_dir_all(".", source).with_context(|| format!("packing {}", source.display()))?;
    } else {
        let name = source.file_name().context("no file name")?;
        b.append_path_with_name(source, name).with_context(|| format!("packing {}", source.display()))?;
    }
    b.finish()?;
    Ok(())
}

/// Unpacks into `dir` and checks every file against the manifest (the client's side).
pub fn unpack_verified(archive: impl Read, dir: &Path, manifest: &Manifest) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut ar = tar::Archive::new(archive);
    ar.set_preserve_permissions(true);
    // unpack_in refuses paths that would leave `dir`.
    for entry in ar.entries()? {
        let mut entry = entry?;
        if !entry.unpack_in(dir)? {
            bail!("the archive tried to write outside its folder");
        }
    }
    for f in &manifest.files {
        let file = std::fs::File::open(dir.join(&f.path)).with_context(|| format!("{} is missing", f.path))?;
        if sha256_of(file)?.0 != f.sha256 {
            bail!("{} doesn't match its checksum; the download is broken", f.path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_with(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data, mode) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(*mode);
            h.set_entry_type(tar::EntryType::Regular);
            // set_path refuses "..", so write the raw name for the hostile cases.
            let name = h.as_old_mut().name.as_mut();
            name[..path.len()].copy_from_slice(path.as_bytes());
            h.set_cksum();
            b.append(&h, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn archives_are_checked() {
        let ok = tar_with(&[("app/run", b"#!/bin/sh\necho hi\n", 0o755), ("app/data.txt", b"x", 0o644)]);
        let m = inspect(ok.as_slice()).unwrap();
        assert_eq!(m.files.len(), 2);
        assert!(m.files[0].executable && !m.files[1].executable);
        assert_eq!(m.total, 19);
        for bad in ["../evil", "/etc/evil", "a/../../evil"] {
            let ar = tar_with(&[(bad, b"x", 0o644)]);
            assert!(inspect(ar.as_slice()).is_err(), "{bad}");
        }
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, "link", "/etc/passwd").unwrap();
        assert!(inspect(b.into_inner().unwrap().as_slice()).is_err(), "links");
    }

    #[test]
    fn pack_unpack_round_trip() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir(src.path().join("lib")).unwrap();
        std::fs::write(src.path().join("lib/x.so"), b"lib").unwrap();
        std::fs::write(src.path().join("app"), b"#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(src.path().join("app"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut ar = Vec::new();
        pack(src.path(), &mut ar).unwrap();
        let m = inspect(ar.as_slice()).unwrap();
        assert!(m.files.iter().any(|f| f.path == "app" && f.executable));
        let out = tempfile::tempdir().unwrap();
        unpack_verified(ar.as_slice(), out.path(), &m).unwrap();
        assert_eq!(std::fs::read(out.path().join("lib/x.so")).unwrap(), b"lib");
        assert!(std::fs::metadata(out.path().join("app")).unwrap().permissions().mode() & 0o100 != 0);
        let mut tampered = m.clone();
        tampered.files[0].sha256 = "00".into();
        assert!(unpack_verified(ar.as_slice(), tempfile::tempdir().unwrap().path(), &tampered).is_err());
    }

    #[test]
    fn names() {
        let t = time::macros::datetime!(2026-11-11 02:15 UTC);
        assert_eq!(build_name(t, "feature/shortcuts dialog", "a1b2c3d"), "2026-11-11-0215-shortcuts-dialog-a1b2c3d");
        assert_eq!(build_name(t, "", ""), "2026-11-11-0215");
        assert!(check_product("imaginarium").is_ok() && check_product("Bad Name").is_err());
    }
}
