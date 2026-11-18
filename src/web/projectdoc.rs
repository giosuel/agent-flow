//! The project document as a dialog over the project's overview (the owner, 2026-11-18: "move the
//! project document out of this section completely and put it into a dialog that can be opened
//! from the project overview"): its sections with contents beside them, its history, and the
//! whole document edited as Markdown with a preview ("lets not do edit by section, just the whole
//! document"). A save over a newer version merges by section (D-201). See `crate::projectdoc`.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Json, Router};
use serde::Deserialize;

use super::editor::MdEditor;
use super::pages::{Nav, nav, render};
use super::{AppResult, AppState, ClientIp};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::projectdoc;
use crate::tz::Tz;
use crate::util::now_ms;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/p/{slug}/doc", get(page).post(save))
        .route("/p/{slug}/doc/preview", post(preview))
        .route("/p/{slug}/doc/rev", get(rev))
}

pub struct SectionView {
    pub index: usize,
    pub title: String,
    /// The text before the first heading has no heading of its own.
    pub intro: bool,
    pub html: String,
}

pub struct RevRow {
    pub rev: i64,
    pub when: String,
    pub who: String,
    pub what: String,
}

#[derive(Template)]
#[template(path = "project_doc.html")]
struct DocPage {
    nav: Nav,
    slug: String,
    name: String,
    owner: bool,
    rev: i64,
    line: String,
    bytes: String,
    sections: Vec<SectionView>,
    editor: Option<MdEditor>,
    history: Vec<RevRow>,
    /// An older revision being looked at: (rev, its sections).
    old: Option<(i64, Vec<SectionView>)>,
    notice: Option<(bool, String)>,
}

pub fn section_views(text: &str) -> Vec<SectionView> {
    projectdoc::sections(text)
        .into_iter()
        .enumerate()
        .map(|(index, (title, body))| {
            let intro = !body.starts_with("## ");
            let rest = if intro { body.as_str() } else { body.split_once('\n').map(|x| x.1).unwrap_or("") };
            SectionView { index, title, intro, html: crate::markdown::to_html(rest) }
        })
        .collect()
}

fn line(d: &projectdoc::Doc, now: i64) -> String {
    if d.rev == 0 {
        String::new()
    } else {
        format!("rev {} · edited {} by {}", d.rev, crate::util::ago(d.updated_at, now), d.author)
    }
}

/// "rev 14 · yesterday", for the overview's link.
pub fn short_line(d: &projectdoc::Doc, now: i64) -> String {
    if d.rev == 0 { String::new() } else { format!("rev {} · {}", d.rev, crate::util::ago(d.updated_at, now)) }
}

#[derive(Deserialize, Default)]
struct DocQuery {
    edit: Option<u8>,
    history: Option<u8>,
    rev: Option<i64>,
    /// How a save went: saved, same, big; `taken` names the agent's sections merged in.
    done: Option<String>,
    taken: Option<String>,
}

fn notice(done: Option<&str>, taken: Option<&str>, agent: &str) -> Option<(bool, String)> {
    let taken = taken.filter(|t| !t.is_empty());
    done.map(|o| match (o, taken) {
        ("saved", Some(t)) => (
            true,
            format!("Saved, with {agent}'s newer version of {t}. {agent} hears of your edit with its next call."),
        ),
        ("saved", None) => (true, format!("Saved. {agent} hears of your edit with its next call.")),
        ("same", _) => (true, "Nothing changed.".to_owned()),
        _ => (false, "That didn't work; nothing was saved.".to_owned()),
    })
}

async fn page(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(slug): Path<String>,
    Query(q): Query<DocQuery>,
    tz: Tz,
) -> AppResult<Response> {
    let owner = actor.is_owner();
    let now = now_ms();
    let s = slug.clone();
    let (p, d, history, old) = state
        .db
        .call(move |c| {
            let p = crate::projects::require(c, &s)?;
            let d = projectdoc::get(c, p.id)?;
            let history = if q.history == Some(1) {
                projectdoc::revisions(c, p.id, 50)?
                    .into_iter()
                    .map(|r| RevRow { rev: r.rev, when: tz.short(r.at), who: r.author, what: r.what })
                    .collect()
            } else {
                Vec::new()
            };
            let old = match q.rev {
                Some(r) if r > 0 && r < d.rev => projectdoc::revision_text(c, p.id, r)?.map(|t| (r, section_views(&t))),
                _ => None,
            };
            Ok((p, d, history, old))
        })
        .await?;
    let editor =
        (owner && q.edit == Some(1)).then(|| doc_editor(&p.slug, d.text.clone(), d.rev, Vec::new(), String::new()));
    render(DocPage {
        nav: nav(&state, &actor, "/projects").await,
        notice: notice(q.done.as_deref(), q.taken.as_deref(), &p.slug),
        slug: p.slug,
        name: p.name,
        owner,
        rev: d.rev,
        line: line(&d, now),
        bytes: format!("{:.1} KB of {} KB", d.text.len() as f64 / 1024.0, projectdoc::MAX_BYTES / 1024),
        sections: section_views(&d.text),
        editor,
        history,
        old,
    })
}

#[derive(Deserialize)]
struct SaveForm {
    /// The revision the owner started from.
    base_rev: i64,
    text: String,
}

enum Saved {
    Done(&'static str, String, String),
    Big(projectdoc::Doc),
    Conflicts(projectdoc::Doc, projectdoc::Merged),
}

async fn save(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path(slug): Path<String>,
    Form(f): Form<SaveForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let s = slug.clone();
    let mine = f.text.replace("\r\n", "\n");
    let mine2 = mine.clone();
    let res = state
        .db
        .call(move |c| {
            let p = crate::projects::require(c, &s)?;
            let cur = projectdoc::get(c, p.id)?;
            let (text, taken) = if f.base_rev == cur.rev {
                (mine2, Vec::new())
            } else {
                // The agent (or another tab) saved meanwhile: merge by section.
                let base = if f.base_rev <= 0 {
                    String::new()
                } else {
                    projectdoc::revision_text(c, p.id, f.base_rev)?.unwrap_or_default()
                };
                let m = projectdoc::merge(&base, &cur.text, &mine2);
                if !m.conflicts.is_empty() {
                    return Ok(Saved::Conflicts(cur, m));
                }
                (m.text, m.taken)
            };
            let what = match projectdoc::changed_sections(&cur.text, &text) {
                w if w.is_empty() => "the whole document".to_owned(),
                w => w,
            };
            match projectdoc::set(c, p.id, &text, "owner", &format!("edited {what}")) {
                Ok((_, true)) => Ok(Saved::Done("saved", what, taken.join(", "))),
                Ok((_, false)) => Ok(Saved::Done("same", what, String::new())),
                Err(e) if e.to_string().contains("KB") => Ok(Saved::Big(cur)),
                Err(e) => Err(e),
            }
        })
        .await?;
    match res {
        Saved::Done(code, what, taken) => {
            if code == "saved" {
                let e = Entry::new(&actor, Via::Page, Action::Write, format!("project document of {slug}: {what}"))
                    .project(slug.clone())
                    .ip(Some(ip.0));
                activity::write(&state.db, e).await;
            }
            let taken = if taken.is_empty() { String::new() } else { format!("&taken={}", super::log::enc(&taken)) };
            Ok(Redirect::to(&format!("/p/{slug}/doc?done={code}{taken}")).into_response())
        }
        // Too big, or both changed a section: the editor again, with the owner's text kept.
        Saved::Big(cur) => editor_again(&state, &actor, &slug, cur, mine, Vec::new(), Some((false, format!(
            "That would make it bigger than {} KB, which is read at the start of every session. Nothing was saved; shorten it, or move long parts into notes.",
            projectdoc::MAX_BYTES / 1024
        ))))
        .await,
        Saved::Conflicts(cur, m) => editor_again(&state, &actor, &slug, cur, m.text, m.conflicts, None).await,
    }
}

/// The project document's editor (`base_rev`: the version the text goes over).
fn doc_editor(
    slug: &str,
    text: String,
    base_rev: i64,
    conflicts: Vec<super::editor::Conflict>,
    other: String,
) -> MdEditor {
    MdEditor {
        action: format!("/p/{slug}/doc"),
        hidden: Vec::new(),
        base_rev,
        preview_html: crate::markdown::to_html(&text),
        text,
        preview_url: format!("/p/{slug}/doc/preview"),
        rev_url: format!("/p/{slug}/doc/rev"),
        conflicts,
        other,
        cancel_href: format!("/p/{slug}/doc"),
        cancel_dlg: true,
        label: "The project document, in Markdown".into(),
        note: format!("{slug} hears of your edit with its next call"),
    }
}

async fn editor_again(
    state: &AppState,
    actor: &Actor,
    slug: &str,
    cur: projectdoc::Doc,
    text: String,
    conflicts: Vec<projectdoc::Conflict>,
    notice: Option<(bool, String)>,
) -> AppResult<Response> {
    let now = now_ms();
    let s = slug.to_owned();
    let p = state.db.call(move |c| crate::projects::require(c, &s)).await?;
    let editor = doc_editor(&p.slug, text, cur.rev, MdEditor::conflicts_of(conflicts), cur.author.clone());
    render(DocPage {
        nav: nav(state, actor, "/projects").await,
        slug: p.slug,
        name: p.name,
        owner: true,
        rev: cur.rev,
        line: line(&cur, now),
        bytes: format!("{:.1} KB of {} KB", cur.text.len() as f64 / 1024.0, projectdoc::MAX_BYTES / 1024),
        sections: Vec::new(),
        editor: Some(editor),
        history: Vec::new(),
        old: None,
        notice,
    })
}

#[derive(Deserialize)]
struct PreviewForm {
    text: String,
}

/// The editor's preview: the Markdown as the dialog shows it.
async fn preview(Extension(actor): Extension<Actor>, Form(f): Form<PreviewForm>) -> AppResult<Response> {
    actor.require_owner()?;
    let text: String = f.text.chars().take(4 * projectdoc::MAX_BYTES).collect();
    Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], crate::markdown::to_html(&text)).into_response())
}

#[derive(Deserialize)]
struct RevQuery {
    since: i64,
}

/// For the editor while it's open: is there a newer version than the one it started from, by whom,
/// and which sections it changed.
async fn rev(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(slug): Path<String>,
    Query(q): Query<RevQuery>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let v = state
        .db
        .call(move |c| {
            let p = crate::projects::require(c, &slug)?;
            let d = projectdoc::get(c, p.id)?;
            let changed = if d.rev > q.since {
                let base = if q.since <= 0 {
                    String::new()
                } else {
                    projectdoc::revision_text(c, p.id, q.since)?.unwrap_or_default()
                };
                projectdoc::changed_sections(&base, &d.text)
            } else {
                String::new()
            };
            Ok(serde_json::json!({"rev": d.rev, "author": d.author, "changed": changed}))
        })
        .await?;
    Ok(Json(v).into_response())
}
