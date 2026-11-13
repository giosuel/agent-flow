//! Builds over HTTP: agents upload them (`flow publish`), the owner's machines download them
//! (`flow run`), and the owner gives feedback on a build's page.

use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Json, Router};
use futures::StreamExt;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;

use super::{AppError, AppResult, AppState, ClientIp, json_error};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::builds::{self, Manifest};
use crate::push::{self, Notification};
use crate::records::{self, Kind, NewRecord, Query as RQuery, Record};
use crate::util::new_secret;
use crate::{mcp, projects};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/builds", get(products).post(upload))
        .route("/api/builds/{product}", get(list))
        .route("/api/builds/{product}/{number}", get(one))
        .route("/api/builds/{product}/{number}/archive", get(archive))
        .route("/p/{slug}/{id}/download", get(download))
        .route("/p/{slug}/{id}/feedback", post(feedback))
        .route("/download/agentflow", get(download_self))
        .route("/download/flow", get(download_self))
}

/// The `flow` program installed next to this server: public, like the install script (the
/// repository is public; flow holds no secrets, a machine gets its token by `flow connect`).
async fn download_self(State(state): State<AppState>) -> Response {
    let path = state.config.flow_bin();
    if !path.is_absolute() {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                (axum::http::header::CONTENT_TYPE, "application/octet-stream"),
                (axum::http::header::CONTENT_DISPOSITION, "attachment; filename=\"flow\""),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn builds_dir(state: &AppState) -> PathBuf {
    builds::dir(&state.config.data_dir())
}

fn free_bytes(dir: &std::path::Path) -> anyhow::Result<u64> {
    let s = rustix::fs::statvfs(dir)?;
    Ok(s.f_bavail.saturating_mul(s.f_frsize))
}

/// An upload's temporary file, removed unless it was kept (also when the client disconnects and
/// the handler is dropped mid-upload).
struct Incoming(PathBuf);

impl Drop for Incoming {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Deserialize)]
struct UploadQuery {
    product: String,
    summary: String,
    #[serde(default)]
    commit: String,
    #[serde(default)]
    branch: String,
    /// The file that starts it; required for a folder, the file's own name for a single binary.
    #[serde(default)]
    entry: String,
    #[serde(default)]
    tag: String,
    /// "Title: what to try", repeatable via `feature=`; JSON list here.
    #[serde(default)]
    features: String,
    /// What it runs on, e.g. "windows-x86_64"; Linux x86_64 if left out.
    #[serde(default)]
    platform: String,
    /// What it needs, e.g. ["glibc 2.39 or newer", "Vulkan"]: a JSON list.
    #[serde(default)]
    needs: String,
    /// The consultant names the project; agents publish to their own.
    project: Option<String>,
    /// An older build moved over (from viber-run): when it was built, "2026-10-26 10:28" UTC.
    #[serde(default)]
    built_at: String,
    /// An older build moved over: the number it had, if it's free.
    number: Option<i64>,
    /// Only check what's asked, without a build: the client asks first, because a refusal in the
    /// middle of a large upload reaches it only as a dropped connection.
    #[serde(default)]
    check: bool,
}

/// The features, sent as a JSON list of "Title: what to do".
fn parse_features(s: &str) -> anyhow::Result<Vec<Value>> {
    if s.trim().is_empty() {
        return Ok(Vec::new());
    }
    let list: Vec<String> = serde_json::from_str(s)?;
    Ok(list.iter().filter_map(|f| builds::feature(f)).collect())
}

/// The product belongs to the caller's project: its `build_product`, or its slug.
fn product_project(
    c: &Connection,
    actor: &Actor,
    product: &str,
    project: Option<&str>,
) -> anyhow::Result<projects::Project> {
    let p = mcp::target_project(c, actor, project, true)?;
    let own = if p.build_product.is_empty() { p.slug.clone() } else { p.build_product.clone() };
    if own != product {
        anyhow::bail!(
            "{} publishes builds as {own:?}, not {product:?} (set build_product with update_project)",
            p.slug
        );
    }
    Ok(p)
}

async fn upload(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<UploadQuery>,
    headers: axum::http::HeaderMap,
    body: Body,
) -> Response {
    if let Err(e) = builds::check_product(&q.product) {
        return json_error(StatusCode::BAD_REQUEST, format!("{e:#}"));
    }
    let checks = builds::check_commit(q.commit.trim())
        .and_then(|_| builds::check_label("branch", &q.branch))
        .and_then(|_| builds::check_label("tag", &q.tag))
        .and_then(|_| builds::check_summary(&q.summary))
        // Stored text is redacted; a branch that looks like a token would no longer match the
        // build's file name.
        .and_then(|_| {
            if crate::activity::redact(&q.branch) != q.branch {
                anyhow::bail!("the branch name looks like it contains a token");
            }
            Ok(())
        })
        .and_then(|_| {
            if q.entry.trim().is_empty() {
                Ok(())
            } else {
                builds::safe_relative(std::path::Path::new(q.entry.trim())).map(|_| ())
            }
        });
    if let Err(e) = checks {
        return json_error(StatusCode::BAD_REQUEST, format!("{e:#}"));
    }
    let features = match parse_features(&q.features) {
        Ok(f) => f,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("features: {e:#}")),
    };
    let built_at = match builds::built_at(&q.built_at, time::OffsetDateTime::now_utc()) {
        Ok(t) => t,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    if let Err(e) = builds::check_history(built_at.is_some(), q.number, !features.is_empty()) {
        return json_error(StatusCode::BAD_REQUEST, format!("{e:#}"));
    }
    let (a, product, project_arg) = (actor.clone(), q.product.clone(), q.project.clone());
    let project = match state.db.call(move |c| product_project(c, &a, &product, project_arg.as_deref())).await {
        Ok(p) => p,
        Err(e) => return json_error(StatusCode::FORBIDDEN, format!("{e:#}")),
    };
    // A taken number is refused before the upload, not after 2 GB of it.
    if let Some(n) = q.number {
        let pid = project.id;
        match state.db.call(move |c| number_taken(c, pid, n)).await {
            Ok(false) => {}
            Ok(true) => return json_error(StatusCode::CONFLICT, format!("build #{n} exists already")),
            Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        }
    }
    if q.check {
        return Json(json!({"ok": true})).into_response();
    }

    // One upload at a time: each may take 2 GB of disk until it's checked.
    let Ok(_uploading) = state.uploads.clone().try_lock_owned() else {
        return json_error(StatusCode::TOO_MANY_REQUESTS, "another build is uploading; try again in a minute");
    };
    // Stream the archive to a file first: builds are large, and nothing is kept unless it checks out.
    let incoming = builds_dir(&state).join(".incoming");
    if let Err(e) = tokio::fs::create_dir_all(&incoming).await {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"));
    }
    // The archive's size if the client said it, else the most it may be.
    let declared: Option<u64> =
        headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
    let limit = declared.unwrap_or(builds::MAX_BYTES).min(builds::MAX_BYTES);
    let floor = state.config.build_free_gb << 30;
    match free_bytes(&incoming) {
        Ok(free) if free < floor + limit => {
            return json_error(
                StatusCode::INSUFFICIENT_STORAGE,
                format!(
                    "the server has {} GB free and keeps {} GB free for other projects; this build doesn't fit",
                    free >> 30,
                    state.config.build_free_gb
                ),
            );
        }
        Ok(_) => {}
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("checking free space: {e:#}")),
    }
    let tmp = Incoming(incoming.join(format!("{}.tar", new_secret(""))));
    let result = async {
        let mut file = tokio::fs::File::create(&tmp.0).await?;
        let mut stream = body.into_data_stream();
        let mut size = 0u64;
        // The upload holds the one upload slot, so a stalled or crawling client can't keep it:
        // a minute without data, or half an hour in all, ends it.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30 * 60);
        loop {
            let next = tokio::time::timeout(std::time::Duration::from_secs(60), stream.next());
            let chunk = match tokio::time::timeout_at(deadline, next).await {
                Err(_) => anyhow::bail!("the upload took longer than 30 minutes"),
                Ok(Err(_)) => anyhow::bail!("the upload stalled for a minute"),
                Ok(Ok(None)) => break,
                Ok(Ok(Some(chunk))) => chunk,
            };
            let chunk = chunk.map_err(|e| anyhow::anyhow!("upload broke off: {e}"))?;
            size += chunk.len() as u64;
            if size > limit {
                anyhow::bail!("the build is larger than 2 GB (or than it said)");
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        let path = tmp.0.clone();
        let manifest = tokio::task::spawn_blocking(move || builds::inspect(std::fs::File::open(&path)?)).await??;
        anyhow::Ok(manifest)
    }
    .await;
    let manifest = match result.and_then(|m| pick_entry(&m, q.entry.trim(), q.platform.trim().is_empty() || q.platform.trim() == builds::DEFAULT_PLATFORM).map(|e| (m, e))) {
        Ok(m) => m,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    let (manifest, entry) = manifest;

    let dir = builds_dir(&state).join(&q.product);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"));
    }
    // The name is taken atomically: a hard link fails if it exists, so two uploads in the same
    // minute from the same commit never share one, and a build never changes.
    let base = builds::build_name(built_at.unwrap_or_else(time::OffsetDateTime::now_utc), q.branch.trim(), q.commit.trim());
    let mut name = base.clone();
    let mut n = 1;
    loop {
        match std::fs::hard_link(&tmp.0, builds::archive_path(&builds_dir(&state), &q.product, &name)) {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => {
                n += 1;
                name = format!("{base}-{n}");
            }
            Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
        }
    }
    drop(tmp);
    let archive = builds::archive_path(&builds_dir(&state), &q.product, &name);
    let manifest_file = builds::manifest_path(&builds_dir(&state), &q.product, &name);
    let written =
        serde_json::to_vec(&manifest).map_err(anyhow::Error::from).and_then(|j| Ok(std::fs::write(&manifest_file, j)?));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&archive);
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    }

    let ready = !features.is_empty();
    // What the agent wants tried becomes To test items, carried along until tested (the owner,
    // 2026-11-11); the build itself no longer waits for a feedback form.
    let to_try: Vec<(String, String)> = features
        .iter()
        .map(|f| {
            let t = f.get("title").and_then(Value::as_str).unwrap_or("").to_owned();
            let w = f.get("what_to_try").and_then(Value::as_str).unwrap_or("").to_owned();
            (t, w)
        })
        .filter(|(t, _)| !t.is_empty())
        .collect();
    let platform = if q.platform.trim().is_empty() { builds::DEFAULT_PLATFORM.to_owned() } else { q.platform.trim().to_owned() };
    if let Err(e) = builds::check_platform(&platform) {
        return json_error(StatusCode::BAD_REQUEST, format!("{e:#}"));
    }
    let needs: Vec<String> = if q.needs.trim().is_empty() {
        Vec::new()
    } else {
        match serde_json::from_str::<Vec<String>>(&q.needs) {
            Ok(n) if n.len() <= 12 && n.iter().all(|x| x.chars().count() <= 200) => n.into_iter().map(|x| x.trim().to_owned()).filter(|x| !x.is_empty()).collect(),
            _ => return json_error(StatusCode::BAD_REQUEST, "needs is a list of up to 12 short lines"),
        }
    };
    let (a, p, nm, ent) = (actor.clone(), project.clone(), name.clone(), entry.clone());
    let created = state
        .db
        .call(move |c| {
            let mut data = Map::new();
            data.insert("product".into(), q.product.clone().into());
            data.insert("name".into(), nm.clone().into());
            data.insert("commit".into(), q.commit.trim().into());
            data.insert("branch".into(), q.branch.trim().into());
            data.insert("entry".into(), ent.into());
            data.insert("size".into(), manifest.total.into());
            data.insert("files_count".into(), manifest.files.len().into());
            data.insert("features".into(), features.into());
            data.insert("platform".into(), platform.clone().into());
            if !needs.is_empty() {
                data.insert("needs".into(), needs.clone().into());
            }
            data.insert("ready_for_feedback".into(), false.into());
            if !q.tag.trim().is_empty() {
                data.insert("tag".into(), q.tag.trim().into());
            }
            let r = records::create(
                c,
                p.id,
                &a.name(),
                NewRecord {
                    kind: Some(Kind::Build),
                    number: q.number,
                    title: q.summary.trim().into(),
                    state: Some("published".into()),
                    data,
                    created_at: built_at.map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64),
                    ..Default::default()
                },
            )?;
            let category: String = r.title.chars().take(40).collect();
            for (title, what) in &to_try {
                crate::totest::add(c, p.id, &a.name(), title, what, &category)?;
            }
            Ok(r)
        })
        .await;
    let r = match created {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_file(&archive);
            let _ = std::fs::remove_file(&manifest_file);
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
        }
    };
    let e = Entry::new(&actor, Via::Api, Action::Write, format!("build {} published: {}", r.short_id(), name))
        .project(r.project.clone())
        .target(r.ref_id.clone());
    activity::write(&state.db, e).await;
    if ready {
        notify_build(&state, &project.name, &r);
    }
    let run = if r.str("platform") == builds::DEFAULT_PLATFORM {
        format!("flow run {} {}", r.str("product"), r.number)
    } else {
        format!("download it from {}{}", state.config.public_url.trim_end_matches('/'), r.url())
    };
    Json(json!({"id": r.ref_id, "number": r.number, "name": name, "run": run})).into_response()
}

fn number_taken(c: &Connection, project_id: i64, number: i64) -> anyhow::Result<bool> {
    Ok(c.query_row(
        "SELECT count(*) FROM records WHERE project_id = ?1 AND kind = 'build' AND number = ?2",
        rusqlite::params![project_id, number],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

pub fn notify_build(state: &AppState, project_name: &str, r: &Record) {
    state.push.spawn(
        push::Kind::Build,
        Notification {
            title: format!("{project_name}: build #{} to try", r.number),
            body: r.title.clone(),
            url: r.url(),
            tag: Some(r.ref_id.clone()),
        },
    );
}

/// The file that starts the build: given, or the only file when it's a single binary.
/// The file that starts the build. On Linux it must be executable (flow run starts it); other
/// platforms' builds are downloads, where the mode doesn't travel.
fn pick_entry(m: &Manifest, entry: &str, linux: bool) -> anyhow::Result<String> {
    if entry.is_empty() {
        return match m.files.as_slice() {
            [only] if only.executable || !linux => Ok(only.path.clone()),
            [_] => anyhow::bail!("the file isn't executable"),
            _ => anyhow::bail!("a folder needs --entry, the executable that starts it"),
        };
    }
    let e = entry.trim_start_matches("./");
    match m.files.iter().find(|f| f.path == e) {
        Some(f) if f.executable || !linux => Ok(f.path.clone()),
        Some(_) => anyhow::bail!("{e} isn't executable"),
        None => anyhow::bail!("{e} isn't in the build"),
    }
}

/// A product's builds: only those of the project that publishes as it now.
fn builds_of(c: &Connection, product: &str) -> anyhow::Result<Vec<Record>> {
    let Some(owner) = projects::by_build_product(c, product)? else { return Ok(Vec::new()) };
    let mut list: Vec<Record> = records::query(
        c,
        &RQuery { project_id: Some(owner.id), kinds: vec![Kind::Build], limit: 0, ..Default::default() },
    )?
    .into_iter()
    .filter(|r| r.str("product") == product && r.state != "withdrawn")
    .collect();
    list.sort_by_key(|r| std::cmp::Reverse(r.created_at));
    Ok(list)
}

fn summary(r: &Record) -> Value {
    json!({
        "number": r.number, "name": r.str("name"), "summary": r.title, "commit": r.str("commit"), "branch": r.str("branch"),
        "entry": r.str("entry"), "size": r.data.get("size"), "tag": r.data.get("tag"), "published": crate::util::fmt_utc(r.created_at),
        "platform": if r.str("platform").is_empty() { builds::DEFAULT_PLATFORM } else { r.str("platform") }, "needs": r.data.get("needs"),
        "project": r.project, "page": r.url(),
    })
}

async fn products(State(state): State<AppState>) -> Response {
    let res = state
        .db
        .call(|c| {
            let all = records::query(c, &RQuery { kinds: vec![Kind::Build], limit: 0, ..Default::default() })?;
            let mut by: std::collections::BTreeMap<String, (usize, Option<Record>)> = Default::default();
            let mut owners: std::collections::HashMap<String, Option<i64>> = Default::default();
            for r in all.into_iter().filter(|r| r.state != "withdrawn") {
                let product = r.str("product").to_owned();
                if !owners.contains_key(&product) {
                    owners.insert(product.clone(), projects::by_build_product(c, &product)?.map(|p| p.id));
                }
                if owners[&product] != Some(r.project_id) {
                    continue;
                }
                let e = by.entry(product).or_default();
                e.0 += 1;
                if e.1.as_ref().is_none_or(|l| l.created_at < r.created_at) {
                    e.1 = Some(r);
                }
            }
            Ok(by
                .into_iter()
                .map(|(p, (n, l))| json!({"product": p, "builds": n, "latest": l.as_ref().map(summary)}))
                .collect::<Vec<_>>())
        })
        .await;
    match res {
        Ok(list) => Json(json!({ "products": list })).into_response(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn list(State(state): State<AppState>, Path(product): Path<String>) -> Response {
    match state.db.call(move |c| builds_of(c, &product)).await {
        Ok(list) => Json(json!({"builds": list.iter().map(summary).collect::<Vec<_>>()})).into_response(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// The build, or the error response to send instead (boxed: a Response is large).
async fn find_build(state: &AppState, product: String, number: i64) -> Result<Record, Box<Response>> {
    match state.db.call(move |c| Ok(builds_of(c, &product)?.into_iter().find(|r| r.number == number))).await {
        Ok(Some(r)) => Ok(r),
        Ok(None) => Err(Box::new(json_error(StatusCode::NOT_FOUND, "no such build"))),
        Err(e) => Err(Box::new(json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))),
    }
}

async fn one(State(state): State<AppState>, Path((product, number)): Path<(String, i64)>) -> Response {
    if builds::check_product(&product).is_err() {
        return json_error(StatusCode::BAD_REQUEST, "bad product");
    }
    match find_build(&state, product.clone(), number).await {
        Ok(r) => {
            let path = builds::manifest_path(&builds_dir(&state), &product, r.str("name"));
            let manifest: Manifest = match tokio::fs::read(&path)
                .await
                .map_err(anyhow::Error::from)
                .and_then(|b| Ok(serde_json::from_slice(&b)?))
            {
                Ok(m) => m,
                Err(e) => return json_error(StatusCode::GONE, format!("the build's file list is missing: {e:#}")),
            };
            let mut v = summary(&r);
            v["files"] = serde_json::to_value(&manifest.files).unwrap_or(Value::Null);
            Json(v).into_response()
        }
        Err(res) => *res,
    }
}

async fn archive(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((product, number)): Path<(String, i64)>,
) -> Response {
    if builds::check_product(&product).is_err() {
        return json_error(StatusCode::BAD_REQUEST, "bad product");
    }
    let r = match find_build(&state, product.clone(), number).await {
        Ok(r) => r,
        Err(res) => return *res,
    };
    let path = builds::archive_path(&builds_dir(&state), &product, r.str("name"));
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => return json_error(StatusCode::GONE, format!("the build's archive is missing: {e}")),
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let e = Entry::new(&actor, Via::Api, Action::Read, format!("build {} downloaded", r.short_id()))
        .project(r.project.clone())
        .target(r.ref_id.clone());
    activity::write(&state.db, e).await;
    let mut res = Body::from_stream(tokio_util::io::ReaderStream::new(file)).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/x-tar"));
    if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    res
}

// ---------------------------------------------------------------------------------------------
// Feedback

/// One piece of the owner's feedback, linked to its build.
fn add_feedback(tx: &Connection, build: &Record, title: &str, state: &str, words: &str) -> anyhow::Result<Record> {
    let mut data = Map::new();
    data.insert("owner_words".into(), words.into());
    data.insert("build".into(), build.ref_id.clone().into());
    let fb = records::create(
        tx,
        build.project_id,
        "owner",
        NewRecord {
            kind: Some(Kind::Feedback),
            title: title.into(),
            state: Some(state.into()),
            data,
            ..Default::default()
        },
    )?;
    records::link(tx, &fb, build, "feedback_on", "owner")?;
    Ok(fb)
}

/// The owner's form: per feature `v{i}` = works | problem | skip and `n{i}` = their words, plus
/// a general note.
async fn feedback(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path((slug, id)): Path<(String, String)>,
    Form(form): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let get = |k: &str| form.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone()).unwrap_or_default();
    let note = get("note");
    let (s, i) = (slug.clone(), id.clone());
    let form2 = form.clone();
    let result = state
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            let build = records::find_str(&tx, &format!("{s}/{i}"), None)?;
            if build.kind != Kind::Build {
                anyhow::bail!("{} isn't a build", build.ref_id);
            }
            if build.state == "withdrawn" {
                anyhow::bail!("this build was withdrawn; there's nothing to try");
            }
            let features = build.data.get("features").and_then(Value::as_array).cloned().unwrap_or_default();
            let val = |k: String| form2.iter().find(|(a, _)| *a == k).map(|(_, v)| v.clone()).unwrap_or_default();
            let mut counts = (0, 0, 0);
            let mut made = Vec::new();
            let unmarked = (0..features.len()).filter(|n| val(format!("v{n}")).is_empty()).count();
            if unmarked > 0 {
                anyhow::bail!(
                    "{unmarked} of {} features aren't marked; mark each (Skip is fine) and send again",
                    features.len()
                );
            }
            for (n, f) in features.iter().enumerate() {
                let state = match val(format!("v{n}")).as_str() {
                    "works" => {
                        counts.0 += 1;
                        "works"
                    }
                    "problem" => {
                        counts.1 += 1;
                        "problem"
                    }
                    "skip" => {
                        counts.2 += 1;
                        "skipped"
                    }
                    _ => continue,
                };
                let title = f.get("title").and_then(Value::as_str).unwrap_or("Feature");
                made.push(add_feedback(&tx, &build, title, state, &val(format!("n{n}")))?);
            }
            if !note.trim().is_empty() {
                made.push(add_feedback(&tx, &build, "General note", "note", note.trim())?);
            }
            if made.is_empty() {
                anyhow::bail!("mark at least one feature, or write a note");
            }
            let mut data = Map::new();
            data.insert("ready_for_feedback".into(), false.into());
            records::update(
                &tx,
                &build,
                "owner",
                records::Patch { state: Some("tried".into()), data, quiet: true, ..Default::default() },
            )?;
            records::event(
                &tx,
                Some(build.project_id),
                Some(build.id),
                "build.feedback",
                &format!(
                    "Owner tried build #{}: {} works, {} problem{}, {} skipped{}",
                    build.number,
                    counts.0,
                    counts.1,
                    if counts.1 == 1 { "" } else { "s" },
                    counts.2,
                    if note.trim().is_empty() { String::new() } else { format!(". \"{}\"", note.trim()) }
                ),
                "owner",
            )?;
            tx.commit()?;
            Ok(build)
        })
        .await;
    match result {
        Ok(b) => {
            let e = Entry::new(&actor, Via::Page, Action::Write, format!("feedback on {}", b.ref_id))
                .project(b.project.clone())
                .target(b.ref_id.clone())
                .ip(Some(ip.0));
            activity::write(&state.db, e).await;
            Ok(Redirect::to(&format!("{}?sent=1", b.url())).into_response())
        }
        Err(e) => Err(AppError::BadRequest(format!("{e:#}"))),
    }
}

/// A build for the owner to download (one that isn't for flow run, like a Windows exe): its one
/// file when it's a single file, else the whole archive.
async fn download(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((slug, id)): Path<(String, String)>,
) -> Response {
    let s = slug.clone();
    let found = state
        .db
        .call(move |c| {
            let Ok(rref) = records::RecordRef::parse(&format!("{s}/{id}")) else { return Ok(None) };
            Ok(records::try_find(c, &rref, None)?.filter(|r| r.kind == Kind::Build))
        })
        .await;
    let Ok(Some(r)) = found else { return StatusCode::NOT_FOUND.into_response() };
    let dir = builds_dir(&state);
    let (product, name, entry) = (r.str("product").to_owned(), r.str("name").to_owned(), r.str("entry").to_owned());
    let archive = builds::archive_path(&dir, &product, &name);
    let manifest: Option<Manifest> =
        std::fs::read(builds::manifest_path(&dir, &product, &name)).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let single = manifest.as_ref().is_some_and(|m| m.files.len() == 1);
    let bytes = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        if !single {
            return Ok(std::fs::read(&archive)?);
        }
        let mut t = tar::Archive::new(std::fs::File::open(&archive)?);
        for e in t.entries()? {
            let mut e = e?;
            if e.header().entry_type().is_file() {
                let mut out = Vec::new();
                std::io::Read::read_to_end(&mut e, &mut out)?;
                return Ok(out);
            }
        }
        anyhow::bail!("the archive is empty")
    })
    .await;
    let Ok(Ok(bytes)) = bytes else { return StatusCode::GONE.into_response() };
    let file: String = if single { entry.rsplit('/').next().unwrap_or(&entry).to_owned() } else { format!("{name}.tar") };
    let file: String = file.chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).collect();
    let e = Entry::new(&actor, Via::Page, Action::Read, format!("build {} downloaded", r.short_id())).project(r.project.clone()).target(r.ref_id.clone());
    activity::write(&state.db, e).await;
    let mut res = bytes.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{file}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    res
}
