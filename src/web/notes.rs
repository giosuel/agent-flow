//! Notes over HTTP: agents upload files (`flow note`), pages offer them for download.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{AppState, json_error};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::notes;
use crate::records::{self, Kind, NewRecord, Patch, RecordRef};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/notes", post(upload).layer(DefaultBodyLimit::max(notes::MAX_BYTES + 1)))
        .route("/p/{slug}/{id}/file", get(file))
}

#[derive(Deserialize)]
struct UploadQuery {
    name: String,
    category: String,
    /// The file's own name, for its extension and the download.
    filename: String,
    #[serde(default)]
    description: String,
    /// A record of the project it belongs to, e.g. T-12 or a step's name isn't a record: use the
    /// description for those.
    #[serde(default)]
    record: String,
    #[serde(default)]
    resurface: String,
    /// A new version of this note's file, e.g. N-6.
    #[serde(default)]
    replaces: String,
}

async fn upload(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    let name = q.name.split_whitespace().collect::<Vec<_>>().join(" ");
    if q.replaces.trim().is_empty() && (name.is_empty() || name.chars().count() > 120) {
        return json_error(StatusCode::BAD_REQUEST, "give it a name (up to 120 characters)");
    }
    if body.len() > notes::MAX_BYTES {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "a note's file can be 25 MB at most");
    }
    if body.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "the file is empty");
    }
    let category = if q.replaces.trim().is_empty() || !q.category.trim().is_empty() {
        match crate::screenshots::category(&q.category) {
            Ok(c) => Some(c),
            Err(_) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "give it a category, e.g. morning report, estimate, known issue",
                );
            }
        }
    } else {
        None
    };
    let resurface = match q.resurface.trim() {
        "" => None,
        r => match notes::resurface(r) {
            Ok(x) => Some(x),
            Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
        },
    };
    let data_dir = state.config.data_dir();
    let free = rustix::fs::statvfs(&data_dir).map(|s| s.f_bavail.saturating_mul(s.f_frsize)).unwrap_or(0);
    if free < (state.config.build_free_gb << 30) + body.len() as u64 {
        return json_error(StatusCode::INSUFFICIENT_STORAGE, "the server's disk is nearly full; not storing it");
    }
    let ext = notes::ext_of(&q.filename);
    let incoming = notes::dir(&data_dir).join(".incoming");
    let tmp = incoming.join(format!("{}.{ext}", crate::util::new_secret("")));
    let written = async {
        tokio::fs::create_dir_all(&incoming).await?;
        tokio::fs::write(&tmp, &body).await
    }
    .await;
    if let Err(e) = written {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"));
    }
    let (a, size, tmp2, dd) = (actor.clone(), body.len(), tmp.clone(), data_dir.clone());
    let filename: String = q.filename.chars().filter(|c| !c.is_control()).take(120).collect();
    let created = state
        .db
        .call(move |c| {
            let project = crate::mcp::target_project(c, &a, None, true)?;
            let mut data = Map::new();
            if let Some(cat) = &category {
                data.insert("category".into(), cat.clone().into());
            }
            data.insert("ext".into(), ext.clone().into());
            data.insert("size".into(), size.into());
            data.insert("filename".into(), filename.clone().into());
            if let Some((at, text)) = &resurface {
                data.insert("resurface".into(), text.clone().into());
                data.insert("resurface_at".into(), at.map(Value::from).unwrap_or(Value::Null));
            }
            // A new version: the old file is kept beside it, numbered.
            if !q.replaces.trim().is_empty() {
                let old = records::find_str(c, q.replaces.trim(), Some(&project.slug))?;
                if old.kind != Kind::Note || old.project_id != project.id {
                    anyhow::bail!("{} isn't a note of {}", old.short_id(), project.slug);
                }
                if old.str("ext").is_empty() {
                    anyhow::bail!("{} is a text note; change it with update_note", old.short_id());
                }
                let version = old.data.get("versions").and_then(Value::as_i64).unwrap_or(0) + 1;
                let (from, to) = (
                    notes::path(&dd, &project.slug, old.number, old.str("ext"), 0),
                    notes::path(&dd, &project.slug, old.number, old.str("ext"), version),
                );
                std::fs::rename(&from, &to)?;
                data.insert("versions".into(), version.into());
                let new_path = notes::path(&dd, &project.slug, old.number, &ext, 0);
                std::fs::rename(&tmp2, &new_path)?;
                let title = if name.is_empty() { None } else { Some(name.clone()) };
                let (r, _) = records::update(
                    c,
                    &old,
                    &a.name(),
                    Patch { title, data, note: format!("new version of the file ({filename})"), ..Default::default() },
                )?;
                return Ok(r);
            }
            // A record (T-12) or a step (P9.R): a note can belong to either.
            let (shows, step) = crate::notes::target(c, &project, q.record.trim())?;
            let r = records::create(
                c,
                project.id,
                &a.name(),
                NewRecord {
                    kind: Some(Kind::Note),
                    title: name,
                    body: q.description.trim().to_owned(),
                    milestone_id: step,
                    data,
                    ..Default::default()
                },
            )?;
            let to = notes::path(&dd, &project.slug, r.number, &ext, 0);
            std::fs::create_dir_all(to.parent().unwrap_or(&dd))?;
            if let Err(e) = std::fs::rename(&tmp2, &to) {
                let patch = Patch {
                    state: Some("resolved".into()),
                    note: "the file couldn't be stored".into(),
                    ..Default::default()
                };
                records::update(c, &r, &a.name(), patch)?;
                return Err(e.into());
            }
            if let Some(target) = &shows {
                records::link(c, &r, target, "relates", &a.name())?;
            }
            Ok(r)
        })
        .await;
    let r = match created {
        Ok(r) => r,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return json_error(StatusCode::BAD_REQUEST, format!("{e:#}"));
        }
    };
    let e = Entry::new(&actor, Via::Api, Action::Write, format!("note {}: {}", r.short_id(), r.title))
        .project(r.project.clone())
        .target(r.ref_id.clone());
    activity::write(&state.db, e).await;
    let url = format!("{}{}", state.config.public_url.trim_end_matches('/'), r.url());
    Json(json!({"id": r.ref_id, "number": r.number, "url": url})).into_response()
}

#[derive(Deserialize)]
struct FileQuery {
    /// An older version.
    v: Option<i64>,
}

/// A note's file, always as a download (text types as plain text), never as a page.
async fn file(
    State(state): State<AppState>,
    Path((slug, id)): Path<(String, String)>,
    Query(q): Query<FileQuery>,
) -> Response {
    let s = slug.clone();
    let found = state
        .db
        .call(move |c| {
            let Ok(rref) = RecordRef::parse(&format!("{s}/{id}")) else { return Ok(None) };
            Ok(records::try_find(c, &rref, None)?.filter(|r| r.kind == Kind::Note))
        })
        .await;
    let Ok(Some(r)) = found else { return StatusCode::NOT_FOUND.into_response() };
    let ext = r.str("ext").to_owned();
    if ext.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let v = q.v.unwrap_or(0).max(0);
    let path = notes::path(&state.config.data_dir(), &slug, r.number, &ext, v);
    let Ok(bytes) = tokio::fs::read(&path).await else { return StatusCode::NOT_FOUND.into_response() };
    let mime = if notes::readable(&ext) { "text/plain; charset=utf-8" } else { "application/octet-stream" };
    let name: String = r.str("filename").chars().filter(|c| c.is_ascii_alphanumeric() || "-_. ".contains(*c)).collect();
    let name = if name.trim().is_empty() { format!("N-{}.{ext}", r.number) } else { name };
    let mut res = bytes.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox; default-src 'none'"));
    res
}

/// A note's text for its page: a text note's body, or a readable file (up to 1 MB).
pub fn text_of(data_dir: &std::path::Path, r: &records::Record) -> Option<(String, bool)> {
    let ext = r.str("ext");
    if ext.is_empty() {
        return Some((r.body.clone(), true));
    }
    if !notes::readable(ext) || r.data.get("size").and_then(Value::as_i64).unwrap_or(0) > 1024 * 1024 {
        return None;
    }
    let bytes = std::fs::read(notes::path(data_dir, &r.project, r.number, ext, 0)).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    Some((text, matches!(ext, "md" | "markdown")))
}

/// A note in the Notes tab's list.
pub struct NoteRow {
    pub short: String,
    pub url: String,
    pub title: String,
    pub category: String,
    /// What the file icon says: md, pdf, csv… or "!" for a known issue, "txt" for a text note.
    pub ext: String,
    pub issue: bool,
    pub linked: String,
    pub resurface: String,
    pub meta: String,
    pub download: String,
}

pub struct NotesView {
    /// (category, href, count, selected), "All" first.
    pub pills: Vec<(String, String, usize, bool)>,
    pub current: Vec<NoteRow>,
    pub resolved: Vec<NoteRow>,
}

pub fn view(
    c: &rusqlite::Connection,
    slug: &str,
    project_id: i64,
    pick: Option<&str>,
    now: i64,
) -> anyhow::Result<NotesView> {
    let all = records::query(
        c,
        &records::Query { project_id: Some(project_id), kinds: vec![Kind::Note], limit: 0, ..Default::default() },
    )?;
    let row = |r: &records::Record| -> anyhow::Result<NoteRow> {
        let ext = r.str("ext").to_owned();
        let cat = r.str("category").to_owned();
        let issue = cat.contains("issue");
        let linked: Vec<String> =
            records::links_of(c, r)?.iter().filter(|l| l.outgoing).map(|l| l.other.short_id()).collect();
        let size = r.data.get("size").and_then(Value::as_i64).map(notes::size).unwrap_or_else(|| "text".into());
        let versions = r.data.get("versions").and_then(Value::as_i64).unwrap_or(0);
        let mut meta = vec![size, crate::util::ago(r.updated_at, now)];
        if versions > 0 {
            meta.insert(1, format!("{} versions", versions + 1));
        }
        Ok(NoteRow {
            short: r.short_id(),
            url: r.url(),
            title: r.title.clone(),
            category: cat,
            ext: if issue {
                "!".into()
            } else if ext.is_empty() {
                "txt".into()
            } else {
                ext.clone()
            },
            issue,
            linked: linked.join(", "),
            resurface: match (r.str("resurface"), r.data.get("resurface_at").and_then(Value::as_i64)) {
                ("", _) => String::new(),
                (_, Some(at)) => {
                    format!("comes back {}", crate::tz::Tz::named("UTC").map(|t| t.date(at)).unwrap_or_default())
                }
                (w, None) => format!("comes back {w}"),
            },
            meta: meta.join(" · "),
            download: if ext.is_empty() || notes::readable(&ext) { String::new() } else { format!("{}/file", r.url()) },
        })
    };
    let mut cats: Vec<(String, usize)> = Vec::new();
    for r in all.iter().filter(|r| r.state == "current") {
        let cat = r.str("category").to_owned();
        match cats.iter_mut().find(|x| x.0 == cat) {
            Some(x) => x.1 += 1,
            None => cats.push((cat, 1)),
        }
    }
    cats.sort();
    let base = format!("/p/{slug}?tab=notes");
    let pick = pick.filter(|p| cats.iter().any(|c| c.0 == *p));
    let current_n = all.iter().filter(|r| r.state == "current").count();
    let mut pills = vec![("All".to_owned(), base.clone(), current_n, pick.is_none())];
    for (cat, n) in cats {
        let on = pick == Some(cat.as_str());
        pills.push((cat.clone(), format!("{base}&ncat={}#notes", super::log::enc(&cat)), n, on));
    }
    let mut current = Vec::new();
    let mut resolved = Vec::new();
    for r in &all {
        if pick.is_some_and(|p| r.str("category") != p) {
            continue;
        }
        if r.state == "current" { current.push(row(r)?) } else { resolved.push(row(r)?) }
    }
    Ok(NotesView { pills, current, resolved })
}

/// A note on its own page: its text (a text note, or a readable file), and the file to download.
pub struct NotePage {
    pub text_html: String,
    pub plain: String,
    pub download: String,
    pub filename: String,
    pub size: String,
    pub category: String,
    pub resurface: String,
    pub versions: Vec<(i64, String)>,
}

pub fn page(data_dir: &std::path::Path, r: &records::Record) -> NotePage {
    let ext = r.str("ext");
    let (text_html, plain) = match text_of(data_dir, r) {
        Some((t, true)) => (crate::markdown::to_html(&t), String::new()),
        Some((t, false)) => (String::new(), t),
        None => (String::new(), String::new()),
    };
    let versions = r.data.get("versions").and_then(Value::as_i64).unwrap_or(0);
    NotePage {
        text_html,
        plain,
        download: if ext.is_empty() { String::new() } else { format!("{}/file", r.url()) },
        filename: r.str("filename").to_owned(),
        size: r.data.get("size").and_then(Value::as_i64).map(notes::size).unwrap_or_default(),
        category: r.str("category").to_owned(),
        resurface: r.str("resurface").to_owned(),
        versions: (1..=versions).rev().map(|v| (v, format!("{}/file?v={v}", r.url()))).collect(),
    }
}
