//! Builders over HTTP (see `crate::builders`): the WebSocket a builder keeps open, the content
//! store `flow remote` uploads to and builders download from, and the results a builder sends
//! back for `flow remote` to fetch.

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;

use super::{AppState, json_error};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::blobs::Store;
use crate::builders::{Reply, hub};

/// Uploads of a working tree's missing files: sources, not builds.
const MAX_PACK: usize = 2 * 1024 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/builders/connect", get(connect))
        .route("/api/builders/blobs", post(blobs_out))
        .route("/api/builders/results/{job}/{name}", post(result_in).layer(DefaultBodyLimit::max(MAX_PACK)))
        .route("/api/remote/blobs/missing", post(missing))
        .route("/api/remote/blobs", post(blobs_in).layer(DefaultBodyLimit::max(MAX_PACK)))
        .route("/api/remote/base", get(base))
        .route("/api/remote/jobs/{id}/results/{name}", get(result_out))
}

pub fn store_dir(state: &AppState) -> std::path::PathBuf {
    state.config.data_dir().join("blobs")
}

fn results_dir(state: &AppState, job: i64) -> std::path::PathBuf {
    state.config.data_dir().join("remote").join("results").join(job.to_string())
}

/// A result's name: the path asked for, made into one file name.
pub fn result_name(path: &str) -> String {
    path.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' }).collect::<String>() + ".tar.zst"
}

/// A builder's own name, if the caller is one that's registered as a daemon builder.
async fn builder_of(state: &AppState, actor: &Actor) -> Option<String> {
    let Actor::Builder { name, .. } = actor else { return None };
    let n = name.clone();
    let known: bool = state
        .db
        .call(move |c| Ok(c.query_row("SELECT EXISTS (SELECT 1 FROM build_servers WHERE name = ?1 AND kind = 'daemon')", [&n], |r| r.get(0))?))
        .await
        .unwrap_or(false);
    known.then(|| name.clone())
}

async fn connect(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(name) = builder_of(&state, &actor).await else {
        return json_error(StatusCode::FORBIDDEN, "only a builder's own token connects here");
    };
    ws.on_upgrade(move |socket| serve(state, name, socket))
}

async fn serve(state: AppState, name: String, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let conn = hub().connect(&name, tx);
    tracing::info!("builder {name} connected");
    activity::write(&state.db, Entry::new(&Actor::System("builders"), Via::Build, Action::Login, format!("builder {name} connected"))).await;
    crate::remote::reload(&state).await;
    let mut ping = tokio::time::interval(std::time::Duration::from_secs(20));
    loop {
        tokio::select! {
            req = rx.recv() => {
                let Some(req) = req else { break };
                let Ok(text) = serde_json::to_string(&req) else { continue };
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<Reply>(&t) {
                    Ok(r) => hub().receive(&name, r),
                    Err(e) => tracing::warn!("builder {name} sent something odd: {e}"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
            _ = ping.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    hub().disconnect(&name, conn);
    tracing::info!("builder {name} disconnected");
    activity::write(&state.db, Entry::new(&Actor::System("builders"), Via::Build, Action::Login, format!("builder {name} disconnected"))).await;
}

#[derive(Deserialize)]
struct Shas {
    shas: Vec<String>,
}

/// A builder downloads what it lacks: one pack.
async fn blobs_out(State(state): State<AppState>, Extension(actor): Extension<Actor>, Json(b): Json<Shas>) -> Response {
    if builder_of(&state, &actor).await.is_none() {
        return json_error(StatusCode::FORBIDDEN, "for builders");
    }
    if b.shas.len() > 200_000 {
        return json_error(StatusCode::BAD_REQUEST, "too many at once");
    }
    let dir = store_dir(&state);
    let packed = tokio::task::spawn_blocking(move || -> anyhow::Result<tempfile::NamedTempFile> {
        std::fs::create_dir_all(&dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        Store::new(&dir).pack(&b.shas, tmp.as_file_mut())?;
        Ok(tmp)
    })
    .await;
    match packed {
        Ok(Ok(tmp)) => match tokio::fs::File::open(tmp.path()).await {
            Ok(f) => {
                // The file goes away once it's sent: the open handle keeps it readable.
                drop(tmp);
                let body = Body::from_stream(tokio_util::io::ReaderStream::new(f));
                ([(header::CONTENT_TYPE, "application/zstd")], body).into_response()
            }
            Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
        },
        Ok(Err(e)) => json_error(StatusCode::NOT_FOUND, format!("{e:#}")),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
    }
}

/// `flow remote` asks which of its files the server lacks.
async fn missing(State(state): State<AppState>, Extension(actor): Extension<Actor>, Json(b): Json<Shas>) -> Response {
    if !matches!(actor, Actor::Project { .. }) {
        return json_error(StatusCode::FORBIDDEN, "for projects' agents");
    }
    if b.shas.len() > 200_000 {
        return json_error(StatusCode::BAD_REQUEST, "too many at once");
    }
    let dir = store_dir(&state);
    let missing = Store::new(&dir).missing(b.shas.iter().map(String::as_str));
    Json(json!({"missing": missing})).into_response()
}

/// `flow remote` uploads them: every blob is checked against its hash.
async fn blobs_in(State(state): State<AppState>, Extension(actor): Extension<Actor>, body: Body) -> Response {
    if !matches!(actor, Actor::Project { .. }) {
        return json_error(StatusCode::FORBIDDEN, "for projects' agents");
    }
    let dir = store_dir(&state);
    let tmp = match spool(body, &dir).await {
        Ok(t) => t,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    let got = tokio::task::spawn_blocking(move || Store::new(&dir).unpack(std::fs::File::open(tmp.path())?)).await;
    match got {
        Ok(Ok(n)) => Json(json!({"stored": n})).into_response(),
        Ok(Err(e)) => json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
    }
}

/// Writes a request body to a temporary file in `dir`.
async fn spool(body: Body, dir: &std::path::Path) -> anyhow::Result<tempfile::NamedTempFile> {
    use tokio::io::AsyncWriteExt;
    tokio::fs::create_dir_all(dir).await?;
    let tmp = tempfile::NamedTempFile::new_in(dir)?;
    let mut f = tokio::fs::File::create(tmp.path()).await?;
    let mut stream = body.into_data_stream();
    let mut size = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow::anyhow!("upload broke off: {e}"))?;
        size += chunk.len();
        if size > MAX_PACK {
            anyhow::bail!("larger than 2 GB");
        }
        f.write_all(&chunk).await?;
    }
    f.flush().await?;
    Ok(tmp)
}

#[derive(Deserialize)]
struct BaseQuery {
    /// The builder; the project's own if left out.
    #[serde(default)]
    server: Option<String>,
}

/// The newest commit a builder has of this project, so flow's git bundle carries only what's new.
async fn base(State(state): State<AppState>, Extension(actor): Extension<Actor>, Query(q): Query<BaseQuery>) -> Response {
    let Actor::Project { project_id, .. } = actor else {
        return json_error(StatusCode::FORBIDDEN, "for projects' agents");
    };
    let found = state
        .db
        .call(move |c| {
            let Some(server) = q.server.or(crate::remote::project_server(c, project_id)?) else { return Ok(None) };
            Ok(c.query_row(
                "SELECT commit_sha FROM remote_jobs WHERE project_id = ?1 AND server = ?2 AND commit_sha != '' ORDER BY id DESC LIMIT 1",
                rusqlite::params![project_id, server],
                |r| r.get::<_, String>(0),
            )
            .ok())
        })
        .await;
    match found {
        Ok(sha) => Json(json!({"commit": sha.unwrap_or_default()})).into_response(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// A builder sends back what a finished job was asked to bring back.
async fn result_in(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((job, name)): Path<(i64, String)>,
    body: Body,
) -> Response {
    let Some(builder) = builder_of(&state, &actor).await else {
        return json_error(StatusCode::FORBIDDEN, "for builders");
    };
    let mine = state.db.call(move |c| crate::remote::get(c, job)).await.ok().flatten().is_some_and(|j| j.server == builder);
    if !mine || !name.ends_with(".tar.zst") || name.contains('/') || name.starts_with('.') {
        return json_error(StatusCode::FORBIDDEN, "not this builder's job");
    }
    let dir = results_dir(&state, job);
    let tmp = match spool(body, &dir).await {
        Ok(t) => t,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    match tmp.persist(dir.join(&name)) {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e.error)),
    }
}

/// `flow remote` fetches a job's result.
async fn result_out(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((id, name)): Path<(i64, String)>,
) -> Response {
    let Actor::Project { project_id, .. } = actor else {
        return json_error(StatusCode::FORBIDDEN, "for projects' agents");
    };
    let mine = state.db.call(move |c| crate::remote::get(c, id)).await.ok().flatten().is_some_and(|j| j.project_id == project_id);
    if !mine || name.contains('/') || name.starts_with('.') {
        return json_error(StatusCode::NOT_FOUND, "no such result");
    }
    match tokio::fs::File::open(results_dir(&state, id).join(&name)).await {
        Ok(f) => ([(header::CONTENT_TYPE, "application/zstd")], Body::from_stream(tokio_util::io::ReaderStream::new(f))).into_response(),
        Err(_) => json_error(StatusCode::NOT_FOUND, "no such result"),
    }
}

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/builders", get(list))
        .route("/api/builders/self", axum::routing::delete(remove_self))
        .route("/api/builders/edit/{name}", axum::routing::patch(edit))
}

/// `flow builder list`: every builder, online or not, with its limits.
async fn list(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> Response {
    if !matches!(actor, Actor::Device { .. } | Actor::Owner { .. }) {
        return json_error(StatusCode::FORBIDDEN, "for your machines");
    }
    let servers = crate::remote::servers(&state);
    let live: std::collections::HashMap<String, bool> =
        state.remote.lock().unwrap_or_else(|e| e.into_inner()).servers.iter().map(|(k, v)| (k.clone(), v.probe.online)).collect();
    let jobs = state
        .db
        .call(|c| {
            let mut st = c.prepare("SELECT server, count(*) FROM remote_jobs WHERE state IN ('queued', 'running', 'paused') GROUP BY server")?;
            let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<Result<std::collections::HashMap<_, _>, _>>()?;
            Ok(v)
        })
        .await
        .unwrap_or_default();
    let rows: Vec<serde_json::Value> = servers
        .iter()
        .map(|b| {
            let online = if b.kind == "daemon" { hub().is_online(&b.name) } else { live.get(&b.name).copied().unwrap_or(false) };
            let hello = hub().hello(&b.name);
            json!({
                "name": b.name, "kind": b.kind, "online": online,
                "version": hello.as_ref().map(|h| h.0.clone()), "threads": hello.as_ref().map(|h| h.1), "memory_gb": hello.as_ref().map(|h| h.2),
                "min_threads": b.min_threads, "min_memory_gb": b.min_memory_gb, "reserve_memory_gb": b.reserve_memory_gb, "keep_days": b.keep_days,
                "jobs": jobs.get(&b.name).copied().unwrap_or(0),
            })
        })
        .collect();
    Json(json!({"builders": rows})).into_response()
}

#[derive(Deserialize)]
struct Edit {
    name: Option<String>,
    min_threads: Option<f64>,
    min_memory_gb: Option<f64>,
    reserve_memory_gb: Option<f64>,
    keep_days: Option<i64>,
}

/// `flow builder edit`: its name and limits, from a connected machine (the owner, 2026-11-13).
async fn edit(State(state): State<AppState>, Extension(actor): Extension<Actor>, Path(name): Path<String>, Json(e): Json<Edit>) -> Response {
    if !matches!(actor, Actor::Device { .. } | Actor::Owner { .. }) {
        return json_error(StatusCode::FORBIDDEN, "for your machines");
    }
    let Some(mut b) = crate::remote::servers(&state).into_iter().find(|s| s.name == name) else {
        return json_error(StatusCode::NOT_FOUND, format!("no builder called {name}"));
    };
    if let Some(v) = e.min_threads { b.min_threads = v; }
    if let Some(v) = e.min_memory_gb { b.min_memory_gb = v; }
    if let Some(v) = e.reserve_memory_gb { b.reserve_memory_gb = v; }
    if let Some(v) = e.keep_days { b.keep_days = v; }
    let (n, b2) = (name.clone(), b.clone());
    if let Err(err) = state.db.call(move |c| crate::remote::set_limits(c, &n, &b2)).await {
        return json_error(StatusCode::BAD_REQUEST, format!("{err:#}"));
    }
    let mut now_called = name.clone();
    if let Some(new) = e.name.filter(|n| n != &name) {
        let (old, n2) = (name.clone(), new.clone());
        if let Err(err) = state.db.call(move |c| crate::remote::rename_server(c, &old, &n2)).await {
            return json_error(StatusCode::BAD_REQUEST, format!("{err:#}"));
        }
        hub().drop_builder(&name);
        now_called = new;
    }
    crate::remote::reload(&state).await;
    activity::write(&state.db, Entry::new(&actor, Via::Api, Action::Write, format!("builder {name} edited (now {now_called})"))).await;
    Json(json!({"name": now_called})).into_response()
}

/// `flow builder remove` on the builder itself: it leaves, and its token stops working.
async fn remove_self(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> Response {
    let Some(name) = builder_of(&state, &actor).await else {
        return json_error(StatusCode::FORBIDDEN, "a builder removes itself with its own token");
    };
    let n = name.clone();
    if let Err(e) = state.db.call(move |c| crate::remote::remove_server(c, &n)).await {
        return json_error(StatusCode::CONFLICT, format!("{e:#}"));
    }
    hub().drop_builder(&name);
    crate::remote::reload(&state).await;
    activity::write(&state.db, Entry::new(&actor, Via::Api, Action::Write, format!("builder {name} removed itself"))).await;
    Json(json!({"removed": name})).into_response()
}
