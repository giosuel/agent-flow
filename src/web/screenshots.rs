//! Screenshots over HTTP: agents upload them (`flow screenshot`), pages show them.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{Map, json};

use super::{AppState, json_error};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::records::{self, Kind, NewRecord, RecordRef};
use crate::screenshots;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/screenshots", post(upload).layer(DefaultBodyLimit::max(screenshots::MAX_BYTES + 1)))
        .route("/p/{slug}/{id}/image", get(image))
        .route("/p/{slug}/{id}/mockup-state", post(set_state))
}

#[derive(Deserialize)]
struct UploadQuery {
    name: String,
    category: String,
    #[serde(default)]
    description: String,
    /// A record of the project it shows, e.g. T-12 or B-3.
    #[serde(default)]
    record: String,
    /// Only for tokens that may write to more than one project.
    project: Option<String>,
    /// For a mockup: where the design is (a claude.ai artifact) and its state.
    #[serde(default)]
    design_url: String,
    #[serde(default)]
    state: String,
}

async fn upload(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    let name = q.name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() || name.chars().count() > 120 {
        return json_error(StatusCode::BAD_REQUEST, "give it a name (up to 120 characters)");
    }
    if q.description.chars().count() > 4000 {
        return json_error(StatusCode::BAD_REQUEST, "the description is longer than 4000 characters");
    }
    let category = match screenshots::category(&q.category) {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    let design_url = q.design_url.trim().to_owned();
    if !design_url.is_empty()
        && let Err(e) = screenshots::check_design_url(&design_url)
    {
        return json_error(StatusCode::BAD_REQUEST, format!("{e:#}"));
    }
    // A mockup has a state; proposed until the owner approves it.
    let mstate = match (q.state.trim(), category.contains("mockup")) {
        ("", true) => "proposed".to_owned(),
        ("", false) => String::new(),
        (st, _) => {
            if let Err(e) = screenshots::check_mockup_state(st) {
                return json_error(StatusCode::BAD_REQUEST, format!("{e:#}"));
            }
            st.to_owned()
        }
    };
    if body.len() > screenshots::MAX_BYTES {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "a screenshot can be 10 MB at most");
    }
    let Some((mime, ext)) = screenshots::sniff(&body) else {
        return json_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "only PNG, JPEG and WebP images");
    };
    let data_dir = state.config.data_dir();
    let free = rustix::fs::statvfs(&data_dir).map(|s| s.f_bavail.saturating_mul(s.f_frsize)).unwrap_or(0);
    if free < (state.config.build_free_gb << 30) + body.len() as u64 {
        return json_error(StatusCode::INSUFFICIENT_STORAGE, "the server's disk is nearly full; not storing it");
    }

    // The file first, under a temporary name; the record makes it visible.
    let incoming = screenshots::dir(&data_dir).join(".incoming");
    let tmp = incoming.join(format!("{}.{ext}", crate::util::new_secret("")));
    let written = async {
        tokio::fs::create_dir_all(&incoming).await?;
        tokio::fs::write(&tmp, &body).await
    }
    .await;
    if let Err(e) = written {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"));
    }

    let (a, size, dims) = (actor.clone(), body.len(), screenshots::png_size(&body));
    let (tmp2, dd) = (tmp.clone(), data_dir.clone());
    let created = state
        .db
        .call(move |c| {
            let project = crate::mcp::target_project(c, &a, q.project.as_deref(), true)?;
            let shows = match q.record.trim() {
                "" => None,
                r => Some(
                    records::try_find(c, &RecordRef::parse(r)?, Some(&project.slug))?
                        .filter(|r| r.project_id == project.id)
                        .ok_or_else(|| anyhow::anyhow!("{} has no record {r}", project.slug))?,
                ),
            };
            let mut data = Map::new();
            data.insert("category".into(), category.into());
            if !mstate.is_empty() {
                data.insert("mockup_state".into(), mstate.into());
            }
            if !design_url.is_empty() {
                data.insert("design_url".into(), design_url.into());
            }
            data.insert("mime".into(), mime.into());
            data.insert("ext".into(), ext.into());
            data.insert("size".into(), size.into());
            if let Some((w, h)) = dims {
                data.insert("width".into(), w.into());
                data.insert("height".into(), h.into());
            }
            let r = records::create(
                c,
                project.id,
                &a.name(),
                NewRecord {
                    kind: Some(Kind::Screenshot),
                    title: name,
                    body: q.description.trim().to_owned(),
                    data,
                    ..Default::default()
                },
            )?;
            let to = screenshots::path(&dd, &project.slug, r.number, ext);
            std::fs::create_dir_all(to.parent().unwrap_or(&dd))?;
            if let Err(e) = std::fs::rename(&tmp2, &to) {
                let patch = records::Patch { state: Some("withdrawn".into()), ..Default::default() };
                records::update(c, &r, &a.name(), patch)?;
                return Err(e.into());
            }
            if let Some(target) = &shows {
                records::link(c, &r, target, "shows", &a.name())?;
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
    let e = Entry::new(&actor, Via::Api, Action::Write, format!("screenshot {}: {}", r.short_id(), r.title))
        .project(r.project.clone())
        .target(r.ref_id.clone());
    activity::write(&state.db, e).await;
    let url = format!("{}{}", state.config.public_url.trim_end_matches('/'), r.url());
    Json(json!({"id": r.ref_id, "number": r.number, "url": url})).into_response()
}

/// The image itself, for anyone signed in (the owner, or a token: agents read all projects).
async fn image(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((slug, id)): Path<(String, String)>,
) -> Response {
    if matches!(actor, Actor::Anonymous) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let s = slug.clone();
    let found = state
        .db
        .call(move |c| {
            let Ok(rref) = RecordRef::parse(&format!("{s}/{id}")) else { return Ok(None) };
            Ok(records::try_find(c, &rref, None)?.filter(|r| r.kind == Kind::Screenshot))
        })
        .await;
    let Ok(Some(r)) = found else { return StatusCode::NOT_FOUND.into_response() };
    let (ext, mime) = (r.str("ext").to_owned(), r.str("mime").to_owned());
    if !matches!(ext.as_str(), "png" | "jpg" | "webp") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(bytes) = tokio::fs::read(screenshots::path(&state.config.data_dir(), &slug, r.number, &ext)).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut res = bytes.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("image/png")));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=86400"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'"));
    res
}

#[derive(Deserialize)]
struct StateForm {
    state: String,
}

/// The owner moves a mockup on: proposed, approved, built.
async fn set_state(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((slug, id)): Path<(String, String)>,
    axum::Form(f): axum::Form<StateForm>,
) -> crate::web::AppResult<Response> {
    actor.require_owner()?;
    let (s, i, st) = (slug.clone(), id.clone(), f.state.clone());
    let done = state
        .db
        .call(move |c| {
            screenshots::check_mockup_state(&st)?;
            let r = records::find(c, &RecordRef::parse(&format!("{s}/{i}"))?, None)?;
            if r.kind != Kind::Screenshot {
                anyhow::bail!("not a screenshot");
            }
            let mut data = Map::new();
            data.insert("mockup_state".into(), st.clone().into());
            records::update(c, &r, "owner", records::Patch { data, note: format!("the owner set it {st}"), ..Default::default() })?;
            Ok(r)
        })
        .await;
    if let Ok(r) = &done {
        let e = Entry::new(&actor, Via::Page, Action::Write, format!("mockup {} set {}", r.short_id(), f.state))
            .project(r.project.clone())
            .target(r.ref_id.clone());
        activity::write(&state.db, e).await;
    }
    Ok(axum::response::Redirect::to(&format!("/p/{slug}/{id}")).into_response())
}
