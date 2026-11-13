//! Test builds for the owner, folded in from viber-run (the owner, 2026-11-07: "integrate
//! viber-run into agent flow and then rewrite the viber-run client CLI to talk to the agent flow
//! server instead"). An agent publishes a build as a tar archive; AgentFlow checks it and keeps
//! the archive as it came; the owner's `flow-run` downloads, verifies and starts it.
//!
//! Like viber-run: each product's builds are numbered and numbers are never reused; a build never
//! changes once published; its name is `<date>-<time>-<topic>-<commit>`.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Largest build accepted (the archive as uploaded).
pub const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Most files in one build, and the longest path: the manifest is kept and sent whole.
pub const MAX_FILES: usize = 20_000;
pub const MAX_PATH: usize = 1024;
/// Largest GNU long-name or PAX header; the tar crate reads these into memory whole.
const MAX_EXTENSION: u64 = 64 * 1024;

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

/// What a build runs on, e.g. "linux-x86_64" or "windows-x86_64" (Imaginarium's agent, 2026-11-12:
/// a Windows exe mustn't be offered to flow-run on Linux). Older builds without one are Linux.
pub const DEFAULT_PLATFORM: &str = "linux-x86_64";

pub fn check_platform(p: &str) -> anyhow::Result<()> {
    let ok = p.len() <= 40
        && p.split_once('-').is_some_and(|(os, arch)| {
            !os.is_empty() && !arch.is_empty() && os.chars().all(|c| c.is_ascii_lowercase())
                && arch.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        });
    if !ok {
        bail!("a platform is os-arch, e.g. linux-x86_64, windows-x86_64 or linux-aarch64");
    }
    Ok(())
}

/// The platform this program runs on, in the same words.
pub fn this_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// "Linux · x86_64".
pub fn platform_label(p: &str) -> String {
    let (os, arch) = p.split_once('-').unwrap_or((p, ""));
    let os = match os {
        "linux" => "Linux",
        "windows" => "Windows",
        "macos" => "macOS",
        o => o,
    };
    if arch.is_empty() { os.to_owned() } else { format!("{os} · {arch}") }
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

/// One feature the owner should try, from "Title: what to do" (the part after the colon is
/// optional); None for an empty line.
pub fn feature(text: &str) -> Option<serde_json::Value> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (title, what) = text.split_once(':').map(|(a, b)| (a.trim(), b.trim())).unwrap_or((text, ""));
    Some(serde_json::json!({"title": title, "what_to_try": what}))
}

/// A commit as git prints it: 4 to 40 hex digits, or nothing. It becomes part of a file name.
pub fn check_commit(commit: &str) -> anyhow::Result<()> {
    if !commit.is_empty() && !(4..=40).contains(&commit.len()) || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("commit {commit:?} isn't a git commit (4 to 40 hex digits)");
    }
    Ok(())
}

/// When an older build (moved over from viber-run) was built: "2026-10-26 10:28", in UTC, and
/// not after `now`. None when it's left out: the build is from now.
pub fn built_at(raw: &str, now: time::OffsetDateTime) -> anyhow::Result<Option<time::OffsetDateTime>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let f = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]");
    let t = time::PrimitiveDateTime::parse(raw, &f)
        .map_err(|_| anyhow::anyhow!("built_at {raw:?}: write it as 2026-10-26 10:28 (UTC)"))?
        .assume_utc();
    if t > now {
        bail!("built_at {raw:?} is in the future");
    }
    Ok(Some(t))
}

/// An older build keeps its number and time; it isn't news, so it has nothing to try (and the
/// owner isn't notified). A number alone would put a new build among the old ones.
pub fn check_history(built_at: bool, number: Option<i64>, features: bool) -> anyhow::Result<()> {
    if let Some(n) = number {
        if !built_at {
            bail!("a build's number is only given for an older build, with built_at");
        }
        if !(1..=1_000_000).contains(&n) {
            bail!("number {n}: a build number is 1 or more");
        }
    }
    if built_at && features {
        bail!("an older build has nothing new to try; leave out the features");
    }
    Ok(())
}

/// Branches and tags are shown and stored, so keep them short and printable.
pub fn check_label(what: &str, value: &str) -> anyhow::Result<()> {
    if value.len() > 200 || value.chars().any(char::is_control) {
        bail!("{what} must be under 200 characters, on one line");
    }
    Ok(())
}

/// A path inside an archive that stays inside its folder: relative, no `..`, no odd components.
pub fn safe_relative(p: &Path) -> anyhow::Result<String> {
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
    let joined = parts.join("/");
    if joined.len() > MAX_PATH {
        bail!("a path in the build is longer than {MAX_PATH} bytes");
    }
    Ok(joined)
}

/// A first pass over the raw headers, before the tar crate reads them: it would read a GNU long
/// name or a PAX header of any size into memory. Also bounds how many entries there are.
fn check_headers(mut r: impl Read + Seek) -> anyhow::Result<()> {
    let mut block = [0u8; 512];
    let mut entries = 0usize;
    let mut next_size: Option<u64> = None;
    loop {
        match r.read_exact(&mut block) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        if block.iter().all(|&b| b == 0) {
            return Ok(());
        }
        let h = tar::Header::from_byte_slice(&block);
        let t = h.entry_type();
        let size = next_size.take().map_or_else(|| h.entry_size(), Ok)?;
        let extension = t.is_gnu_longname() || t.is_gnu_longlink() || t.is_pax_local_extensions();
        if (extension || t.is_pax_global_extensions()) && size > MAX_EXTENSION {
            bail!("the archive has a {size}-byte extension header; that isn't a normal build");
        }
        if t.is_pax_local_extensions() {
            // A PAX "size" record overrides the next header's size field.
            let mut data = vec![0u8; size as usize];
            r.read_exact(&mut data)?;
            for ext in tar::PaxExtensions::new(&data) {
                let ext = ext?;
                if ext.key_bytes() == b"size" {
                    next_size = Some(ext.value()?.parse().context("PAX size")?);
                }
            }
            r.seek(SeekFrom::Current((size.div_ceil(512) * 512 - size) as i64))?;
            continue;
        }
        if !extension {
            entries += 1;
            if entries > MAX_FILES {
                bail!("the build has more than {MAX_FILES} files");
            }
        }
        let skip = i64::try_from(size.div_ceil(512) * 512).context("entry size")?;
        r.seek(SeekFrom::Current(skip))?;
    }
}

/// Reads a tar archive and lists its regular files with checksums. Refuses anything that could
/// write outside the folder it's unpacked into: absolute paths, `..`, links, devices; and
/// archives too large to list (see the limits above).
pub fn inspect(mut archive: impl Read + Seek) -> anyhow::Result<Manifest> {
    check_headers(&mut archive)?;
    archive.rewind()?;
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
        if files.len() >= MAX_FILES {
            bail!("the build has more than {MAX_FILES} files");
        }
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
    Ok((crate::util::hex(&w.0.finalize()), n))
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
    let topic: String = topic.chars().take(40).collect();
    let topic = if topic.is_empty() || topic.starts_with('.') { "-".to_owned() } else { topic };
    // check_commit has refused anything else already; this keeps the name a plain file name anyway.
    let commit: String = commit.chars().filter(char::is_ascii_hexdigit).take(40).collect();
    format!("{date}-{topic}-{commit}")
}

/// Where builds live on the server, under its data folder.
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("builds")
}

/// Where a build's archive lives on the server.
pub fn archive_path(builds_dir: &Path, product: &str, name: &str) -> PathBuf {
    builds_dir.join(product).join(format!("{name}.tar"))
}

/// Its manifest, kept beside the archive rather than in the record, so listing builds stays small.
pub fn manifest_path(builds_dir: &Path, product: &str, name: &str) -> PathBuf {
    builds_dir.join(product).join(format!("{name}.json"))
}

/// Uploads that never finished (AgentFlow stopped mid-upload): removed at start.
pub fn sweep_incoming(builds_dir: &Path) -> usize {
    let Ok(list) = std::fs::read_dir(builds_dir.join(".incoming")) else { return 0 };
    list.filter_map(Result::ok).filter(|e| std::fs::remove_file(e.path()).is_ok()).count()
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

/// Unpacks into `dir` and checks every file against the manifest (the client's side). Only the
/// listed files are written, as plain files with plain modes (no setuid, no links), whatever the
/// archive says.
pub fn unpack_verified(archive: impl Read, dir: &Path, manifest: &Manifest) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    let listed: std::collections::HashMap<&str, &FileEntry> =
        manifest.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut ar = tar::Archive::new(archive);
    for entry in ar.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind == tar::EntryType::Directory {
            continue;
        }
        let path = safe_relative(&entry.path()?)?;
        if !matches!(kind, tar::EntryType::Regular | tar::EntryType::Continuous) {
            bail!("{path}: the archive holds something other than a file");
        }
        let f = listed.get(path.as_str()).with_context(|| format!("{path} isn't in the build's file list"))?;
        let target = dir.join(&path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .with_context(|| format!("{path} is in the archive twice"))?;
        std::io::copy(&mut entry, &mut out)?;
        let mode = if f.executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
    }
    for f in &manifest.files {
        let file = std::fs::File::open(dir.join(&f.path)).with_context(|| format!("{} is missing", f.path))?;
        if sha256_of(file)?.0 != f.sha256 {
            bail!("{} doesn't match its checksum; the download is broken", f.path);
        }
    }
    Ok(())
}

/// The file to start: listed in the manifest as executable, inside `dir`, and a plain file there.
pub fn entry_path(dir: &Path, manifest: &Manifest, entry: &str) -> anyhow::Result<PathBuf> {
    let rel = safe_relative(Path::new(entry))?;
    if !manifest.files.iter().any(|f| f.path == rel && f.executable) {
        bail!("the build's entry {entry:?} isn't one of its executables");
    }
    let path = dir.join(&rel);
    let meta = std::fs::symlink_metadata(&path).with_context(|| format!("{rel} is missing"))?;
    if !meta.file_type().is_file() {
        bail!("{rel} isn't a plain file");
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platforms() {
        for ok in ["linux-x86_64", "windows-x86_64", "linux-aarch64", "macos-aarch64"] {
            assert!(check_platform(ok).is_ok(), "{ok}");
        }
        for bad in ["linux", "Linux-x86_64", "linux x86", "-x86", "linux-"] {
            assert!(check_platform(bad).is_err(), "{bad}");
        }
        assert_eq!(platform_label("windows-x86_64"), "Windows · x86_64");
    }

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
        let m = inspect(std::io::Cursor::new(&ok)).unwrap();
        assert_eq!(m.files.len(), 2);
        assert!(m.files[0].executable && !m.files[1].executable);
        assert_eq!(m.total, 19);
        for bad in ["../evil", "/etc/evil", "a/../../evil"] {
            let ar = tar_with(&[(bad, b"x", 0o644)]);
            assert!(inspect(std::io::Cursor::new(&ar)).is_err(), "{bad}");
        }
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, "link", "/etc/passwd").unwrap();
        assert!(inspect(std::io::Cursor::new(b.into_inner().unwrap())).is_err(), "links");
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
        let m = inspect(std::io::Cursor::new(&ar)).unwrap();
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
    fn huge_headers_and_odd_commits_are_refused() {
        // A GNU long name of 1 MB would be read into memory whole by the tar crate.
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::GNULongName);
        h.set_size(1 << 20);
        h.set_cksum();
        b.append(&h, vec![b'a'; 1 << 20].as_slice()).unwrap();
        let err = inspect(std::io::Cursor::new(b.into_inner().unwrap())).unwrap_err().to_string();
        assert!(err.contains("extension header"), "{err}");
        // A long but sane path is fine through the same check.
        let long = format!("{}/app", "d".repeat(200));
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o755);
        h.set_size(2);
        b.append_data(&mut h, &long, b"hi".as_slice()).unwrap();
        assert_eq!(inspect(std::io::Cursor::new(b.into_inner().unwrap())).unwrap().files[0].path, long);

        assert!(check_commit("a1b2c3d").is_ok() && check_commit("").is_ok());
        for bad in ["../../x", "abc", "a1b2c3d/", &"a".repeat(41), "zzzz"] {
            assert!(check_commit(bad).is_err(), "{bad}");
        }
        let t = time::macros::datetime!(2026-11-11 02:15 UTC);
        assert_eq!(build_name(t, "..", "../a1b2"), "2026-11-11-0215---a1b2", "never a path");
    }

    #[test]
    fn unpack_writes_only_listed_plain_files() {
        let ar = tar_with(&[("app", b"#!/bin/sh\n", 0o4755), ("extra", b"x", 0o644)]);
        let m = inspect(std::io::Cursor::new(&ar)).unwrap();
        let only_app = Manifest { files: m.files.iter().filter(|f| f.path == "app").cloned().collect(), total: 0 };
        let out = tempfile::tempdir().unwrap();
        let err = unpack_verified(ar.as_slice(), out.path(), &only_app).unwrap_err().to_string();
        assert!(err.contains("extra isn't in the build's file list"), "{err}");
        let out = tempfile::tempdir().unwrap();
        unpack_verified(ar.as_slice(), out.path(), &m).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(out.path().join("app")).unwrap().permissions().mode() & 0o7777,
            0o755,
            "no setuid"
        );
        assert!(entry_path(out.path(), &m, "app").is_ok());
        assert!(entry_path(out.path(), &m, "extra").is_err(), "not executable");
        assert!(entry_path(out.path(), &m, "../app").is_err());
    }

    #[test]
    fn older_builds() {
        let now = time::macros::datetime!(2026-11-13 09:00 UTC);
        assert_eq!(built_at("", now).unwrap(), None);
        assert_eq!(built_at(" 2026-10-26 10:28 ", now).unwrap(), Some(time::macros::datetime!(2026-10-26 10:28 UTC)));
        assert!(built_at("2026-11-14 10:00", now).is_err());
        assert!(built_at("26.10.2026", now).is_err());
        assert!(check_history(true, Some(5), false).is_ok());
        assert!(check_history(false, Some(5), false).is_err());
        assert!(check_history(true, Some(0), false).is_err());
        assert!(check_history(true, None, true).is_err());
        assert!(check_history(false, None, true).is_ok());
        assert_eq!(
            build_name(time::macros::datetime!(2026-10-26 10:28 UTC), "feat/m6.12-text-style", "2925797"),
            "2026-10-26-1028-m6.12-text-style-2925797"
        );
    }

    #[test]
    fn names() {
        let t = time::macros::datetime!(2026-11-11 02:15 UTC);
        assert_eq!(build_name(t, "feature/shortcuts dialog", "a1b2c3d"), "2026-11-11-0215-shortcuts-dialog-a1b2c3d");
        assert_eq!(build_name(t, "", ""), "2026-11-11-0215");
        assert!(check_product("imaginarium").is_ok() && check_product("Bad Name").is_err());
    }
}
