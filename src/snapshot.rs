//! `flow remote`'s side of a build on a builder that connects itself: a snapshot of the checkout
//! (every file git would see, by content), uploaded to the server only where the server lacks it,
//! with new commits and tags as a git bundle; and the results brought back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::json;

use crate::blobs::{Entry, Manifest};
use crate::cli::Client;

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    Ok(std::process::Command::new("git").arg("-C").arg(dir).args(args).output()?)
}

fn git_text(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = git(dir, args)?;
    if !o.status.success() {
        bail!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// The checkout's top folder and where in it `dir` is.
pub fn top_of(dir: &Path) -> anyhow::Result<(PathBuf, String)> {
    let dir = std::fs::canonicalize(dir).with_context(|| format!("{} doesn't exist", dir.display()))?;
    let top = PathBuf::from(git_text(&dir, &["rev-parse", "--show-toplevel"]).context("not in a git checkout")?.trim());
    let sub = dir.strip_prefix(&top).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    Ok((top, sub))
}

/// Hashes earlier rounds computed, by path, size and modification time: an unchanged checkout is
/// not read again.
type Cache = HashMap<String, (u64, i128, String)>;

fn cache_path(top: &Path) -> PathBuf {
    crate::util::expand_home("~/.cache/agentflow/hashes")
        .join(format!("{}.json", crate::usage::folder_for(&top.display().to_string())))
}

/// Every file git would see here (tracked and new, not ignored, that exists), by content.
pub fn manifest(top: &Path) -> anyhow::Result<Manifest> {
    use std::os::unix::fs::MetadataExt;
    let listed = git_text(top, &["ls-files", "-z", "--cached", "--others", "--exclude-standard"])?;
    let mut paths: Vec<&str> = listed.split('\0').filter(|p| !p.is_empty()).collect();
    paths.sort_unstable();
    paths.dedup();
    let cp = cache_path(top);
    let mut cache: Cache = std::fs::read(&cp).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let mut files = Vec::new();
    for p in paths {
        let full = top.join(p);
        let Ok(md) = std::fs::symlink_metadata(&full) else { continue };
        if !md.file_type().is_file() {
            continue;
        }
        let mtime = md.mtime() as i128 * 1_000_000_000 + md.mtime_nsec() as i128;
        let sha = match cache.get(p) {
            Some((size, t, sha)) if *size == md.len() && *t == mtime => sha.clone(),
            _ => {
                let (sha, _) = crate::blobs::hash_file(&full)?;
                cache.insert(p.to_owned(), (md.len(), mtime, sha.clone()));
                sha
            }
        };
        files.push(Entry { path: p.to_owned(), sha, exec: md.mode() & 0o111 != 0, size: md.len() });
    }
    if let Some(d) = cp.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(&cp, serde_json::to_vec(&cache)?);
    let commit = git_text(top, &["rev-parse", "HEAD"]).map(|s| s.trim().to_owned()).unwrap_or_default();
    Ok(Manifest { commit, files })
}

/// New commits and tags since `base` (all of them if the builder has none), or None.
fn bundle(top: &Path, base: &str, commit: &str) -> anyhow::Result<Option<Vec<u8>>> {
    if commit.is_empty() || base == commit {
        return Ok(None);
    }
    let tmp = tempfile::NamedTempFile::new()?;
    let path = tmp.path().display().to_string();
    let known = !base.is_empty() && git(top, &["cat-file", "-e", &format!("{base}^{{commit}}")])?.status.success();
    let mut args = vec!["bundle", "create", "-q", path.as_str(), "HEAD", "--tags"];
    let not = format!("^{base}");
    if known {
        args.push(&not);
    }
    let o = git(top, &args)?;
    if !o.status.success() {
        return Ok(None); // nothing new (an empty bundle is refused)
    }
    Ok(Some(std::fs::read(tmp.path())?))
}

/// What a job needs from `flow remote`: the snapshot's manifest and bundle, uploaded.
pub struct Uploaded {
    pub manifest: String,
    pub bundle: String,
    pub sent_files: usize,
}

/// Where a job would run: (`kind`, online) of the builder, None when the server doesn't say.
pub fn target(client: &Client, server: Option<&str>) -> Option<(String, bool, String)> {
    let v =
        client.get(&format!("/api/remote/base{}", server.map(|s| format!("?server={s}")).unwrap_or_default())).ok()?;
    Some((
        v["kind"].as_str().unwrap_or("").to_owned(),
        v["online"].as_bool().unwrap_or(false),
        v["commit"].as_str().unwrap_or("").to_owned(),
    ))
}

pub fn upload(client: &Client, top: &Path, base: &str) -> anyhow::Result<Uploaded> {
    let m = manifest(top)?;
    let base = base.to_owned();
    let bundle = bundle(top, &base, &m.commit)?;
    let manifest_bytes = serde_json::to_vec(&m)?;
    let manifest_sha = crate::blobs::sha_of(&manifest_bytes);
    let bundle_sha = bundle.as_ref().map(|b| crate::blobs::sha_of(b)).unwrap_or_default();
    let mut wanted: Vec<String> = m.files.iter().map(|e| e.sha.clone()).collect();
    wanted.push(manifest_sha.clone());
    if !bundle_sha.is_empty() {
        wanted.push(bundle_sha.clone());
    }
    wanted.sort();
    wanted.dedup();
    let missing: Vec<String> = client.post_json("/api/remote/blobs/missing", &json!({"shas": wanted}))?["missing"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let missing: std::collections::HashSet<String> = missing.into_iter().collect();
    let files: Vec<&Entry> = m.files.iter().filter(|e| missing.contains(&e.sha)).collect();
    let mut extra = Vec::new();
    if missing.contains(&manifest_sha) {
        extra.push((manifest_sha.clone(), manifest_bytes));
    }
    if let Some(b) = bundle
        && missing.contains(&bundle_sha)
    {
        extra.push((bundle_sha.clone(), b));
    }
    if !files.is_empty() || !extra.is_empty() {
        // Files first, then the small ones, in one pack.
        let tmp = tempfile::NamedTempFile::new_in(crate::util::expand_home("~/.cache/agentflow"))
            .or_else(|_| tempfile::NamedTempFile::new())?;
        {
            let enc = zstd::stream::write::Encoder::new(std::fs::File::create(tmp.path())?, 3)?.auto_finish();
            let mut tar = tar::Builder::new(enc);
            let mut done = std::collections::HashSet::new();
            for e in &files {
                if !done.insert(e.sha.clone()) {
                    continue;
                }
                let mut f = std::fs::File::open(top.join(&e.path))?;
                let mut h = tar::Header::new_gnu();
                h.set_size(f.metadata()?.len());
                h.set_mode(0o644);
                h.set_cksum();
                tar.append_data(&mut h, &e.sha, &mut f)?;
            }
            for (sha, data) in &extra {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                tar.append_data(&mut h, sha, data.as_slice())?;
            }
            tar.finish()?;
        }
        let up = Client::with_token(&client.base, &client.token).with_timeout(Duration::from_secs(1800));
        up.post_file_as("/api/remote/blobs", std::fs::File::open(tmp.path())?, "application/zstd")?;
    }
    Ok(Uploaded { manifest: manifest_sha, bundle: bundle_sha, sent_files: files.len() })
}

/// Brings back what the job sent: each `--fetch` path into the checkout.
pub fn fetch_results(
    client: &Client,
    id: i64,
    top: &Path,
    fetch: &[String],
    results: &[String],
) -> anyhow::Result<Vec<String>> {
    let mut got = Vec::new();
    for f in fetch {
        let name = crate::web::builders::result_name(f);
        if !results.contains(&name) {
            continue;
        }
        let tmp = tempfile::NamedTempFile::new()?;
        client.download(&format!("/api/remote/jobs/{id}/results/{name}"), tmp.path())?;
        let parent = top.join(Path::new(f).parent().unwrap_or(Path::new("")));
        std::fs::create_dir_all(&parent)?;
        let base = Path::new(f).file_name().and_then(|n| n.to_str()).unwrap_or("");
        // What's there now goes, so the result is exactly what came back.
        let target = parent.join(base);
        if target.is_dir() {
            let _ = std::fs::remove_dir_all(&target);
        }
        let dec = zstd::stream::read::Decoder::new(std::fs::File::open(tmp.path())?)?;
        let mut ar = tar::Archive::new(dec);
        ar.set_preserve_permissions(true);
        let mut total = 0u64;
        for entry in ar.entries()? {
            let mut e = entry?;
            let p = e.path()?.to_path_buf();
            crate::builds::safe_relative(&p)?;
            if !p.starts_with(base) {
                bail!("the result holds {}, not {base}", p.display());
            }
            total += e.size();
            if total > crate::blobs::MAX_PACK {
                bail!("the result unpacks to more than {} GB", crate::blobs::MAX_PACK >> 30);
            }
            // Links stay inside the result: relative, without `..`.
            if let Some(target) = e.link_name()? {
                let kind = e.header().entry_type();
                if (kind.is_symlink() || kind.is_hard_link()) && crate::builds::safe_relative(&target).is_err() {
                    bail!("the result has a link out of it: {} -> {}", p.display(), target.display());
                }
            }
            e.unpack_in(&parent)?;
        }
        got.push(f.clone());
    }
    Ok(got)
}
