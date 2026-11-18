//! The Handbook page (once Guides): the handbook's pages, read like the project document (contents
//! beside the text) and edited the same way, the whole file as Markdown with a preview (the owner,
//! 2026-11-18: "make the handbook the same as the project document, not have each section edited
//! manually, but in the markdown editor instead with the preview. reuse the components"). Every
//! save is a revision kept by AgentFlow and its backup, no git; a save over the consultant's newer
//! edit merges by section. `GET /api/handbook` hands every file to `flow` for each machine's copy.

use askama::Template;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::editor::MdEditor;
use super::log::enc;
use super::pages::{Nav, nav, render};
use super::{AppError, AppResult, AppState, ClientIp};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::tz::Tz;
use crate::{handbook, markdown};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/handbook", get(page).post(save))
        .route("/handbook/preview", post(preview))
        .route("/handbook/rev", get(rev))
        .route("/api/handbook", get(api_bundle))
}

struct FileRow {
    name: String,
    blurb: String,
    selected: bool,
}

struct SectionView {
    index: usize,
    title: String,
    /// The text before the first heading has no heading of its own.
    intro: bool,
    html: String,
}

struct RevRow {
    rev: i64,
    when: String,
    who: String,
    what: String,
}

#[derive(Template)]
#[template(path = "handbook.html")]
struct HandbookPage {
    nav: Nav,
    files: Vec<FileRow>,
    /// The file shown; on the phone, a file picked (not just the first shown).
    file: Option<String>,
    picked: bool,
    line: String,
    sections: Vec<SectionView>,
    editor: Option<MdEditor>,
    history: Vec<RevRow>,
    /// An older revision being looked at: (rev, its sections).
    old: Option<(i64, Vec<SectionView>)>,
    rev: i64,
    notice: Option<(bool, String)>,
}

#[derive(Deserialize)]
struct PageQuery {
    f: Option<String>,
    edit: Option<u8>,
    history: Option<u8>,
    rev: Option<i64>,
    saved: Option<i64>,
    taken: Option<String>,
    error: Option<String>,
}

/// The first plain line of a file, for the list, with Markdown marks and links' targets removed.
fn blurb(text: &str) -> String {
    let line =
        text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('@')).unwrap_or("");
    let mut out = String::new();
    let mut rest = line;
    // "[OWNER.md](OWNER.md)" → "OWNER.md"
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match (after.find("]("), after.find(')')) {
            (Some(close), Some(end)) if close < end => {
                out.push_str(&after[..close]);
                rest = &after[end + 1..];
            }
            _ => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    crate::activity::clip(&out.replace(['*', '`'], ""), 90)
}

/// A section's text without its heading line: the card shows the title already.
fn without_heading(text: &str) -> &str {
    match text.split_once('\n') {
        Some((first, rest)) if first.starts_with('#') => rest,
        None if text.starts_with('#') => "",
        _ => text,
    }
}

fn section_views(f: &handbook::File) -> Vec<SectionView> {
    f.sections
        .iter()
        .enumerate()
        .map(|(i, s)| SectionView {
            index: i,
            title: s.title.clone(),
            intro: s.level < 2,
            html: local_links(&markdown::to_html(if s.level < 2 { &s.text } else { without_heading(&s.text) })),
        })
        .collect()
}

/// Links from one page to another ("[OWNER.md](OWNER.md)", "MACHINE.md#ports") open it here.
fn local_links(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.find("href=\"") {
        let (head, tail) = rest.split_at(i + 6);
        out.push_str(head);
        let end = tail.find('"').unwrap_or(tail.len());
        let target = &tail[..end];
        let (file, anchor) = target.split_once('#').unwrap_or((target, ""));
        if handbook::is_page(file) && handbook::check_path(file).is_ok() {
            out.push_str(&format!("/handbook?f={file}"));
            if !anchor.is_empty() {
                out.push('#');
                out.push_str(anchor);
            }
        } else {
            out.push_str(target);
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// The handbook file's editor (`base_rev`: the revision the text goes over).
fn file_editor(
    name: &str,
    text: String,
    base_rev: i64,
    conflicts: Vec<super::editor::Conflict>,
    other: String,
) -> MdEditor {
    MdEditor {
        action: "/handbook".into(),
        hidden: vec![("f", name.to_owned())],
        base_rev,
        preview_html: local_links(&markdown::to_html(&text)),
        text,
        preview_url: "/handbook/preview".into(),
        rev_url: format!("/handbook/rev?f={}", enc(name)),
        conflicts,
        other,
        cancel_href: format!("/handbook?f={}", enc(name)),
        cancel_dlg: false,
        label: format!("{name}, in Markdown"),
        note: "Agents read it right away; every machine's copy follows at its next session start".into(),
    }
}

async fn page(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<PageQuery>,
    tz: Tz,
) -> AppResult<Response> {
    actor.require_owner()?;
    let picked = q.f.as_deref().is_some_and(handbook::is_page);
    let want = q.f.clone().filter(|f| handbook::is_page(f));
    let (rows, file, history, old) = state
        .db
        .call(move |c| {
            let names = handbook::pages(c)?;
            let rows: Vec<(String, String)> = names
                .iter()
                .map(|n| (n.clone(), handbook::read(c, n).map(|g| blurb(&g.text)).unwrap_or_default()))
                .collect();
            // On the desktop, the first page shows when none is picked.
            let name = want.or_else(|| names.first().cloned());
            let file = match &name {
                Some(n) => handbook::read(c, n).ok(),
                None => None,
            };
            let history = match (&file, q.history) {
                (Some(f), Some(1)) => handbook::revisions(c, &f.name, 50)?,
                _ => Vec::new(),
            };
            let old = match (&file, q.rev) {
                (Some(f), Some(r)) if r > 0 && r < f.rev => handbook::revision_text(c, &f.name, r)?.map(|t| {
                    let g = handbook::File {
                        name: f.name.clone(),
                        sections: handbook::split(&t),
                        text: t,
                        rev: r,
                        updated_at: 0,
                        author: String::new(),
                    };
                    (r, section_views(&g))
                }),
                _ => None,
            };
            Ok((rows, file, history, old))
        })
        .await?;
    let editor = match (&file, q.edit) {
        (Some(f), Some(1)) => Some(file_editor(&f.name, f.text.clone(), f.rev, Vec::new(), String::new())),
        _ => None,
    };
    let now = crate::util::now_ms();
    let notice = match (q.saved, q.error.as_deref()) {
        (Some(r), _) => Some((
            true,
            match q.taken.as_deref().filter(|t| !t.is_empty()) {
                Some(t) => {
                    format!("Saved as rev {r}, with the newer version of {t} merged in. Agents read it from now on.")
                }
                None => format!("Saved as rev {r}. Agents read it from now on."),
            },
        )),
        (None, Some("same")) => Some((true, "Nothing changed.".to_owned())),
        (None, Some(_)) => Some((false, "That didn't work; nothing was saved. See the log.".to_owned())),
        _ => None,
    };
    let page_nav = nav(&state, &actor, "/handbook").await;
    render(HandbookPage {
        nav: if picked { page_nav.back_to("/handbook", "Handbook") } else { page_nav },
        files: rows
            .into_iter()
            .map(|(n, b)| FileRow { selected: file.as_ref().is_some_and(|f| f.name == n), name: n, blurb: b })
            .collect(),
        picked,
        line: file
            .as_ref()
            .map(|f| format!("rev {} · edited {} by {}", f.rev, crate::util::ago(f.updated_at, now), f.author))
            .unwrap_or_default(),
        sections: file.as_ref().map(section_views).unwrap_or_default(),
        rev: file.as_ref().map(|f| f.rev).unwrap_or(0),
        file: file.map(|f| f.name),
        editor,
        history: history
            .into_iter()
            .map(|r| RevRow { rev: r.rev, when: tz.short(r.at), who: r.author, what: r.what })
            .collect(),
        old,
        notice,
    })
}

#[derive(Deserialize)]
struct SaveForm {
    f: String,
    /// The revision the owner started from.
    base_rev: i64,
    text: String,
}

enum Saved {
    Done(i64, String, String),
    Same,
    Conflicts(handbook::File, crate::projectdoc::Merged),
}

/// Saves the whole file. Over a newer revision (the consultant's), it merges by section like the
/// project document: the owner's edits stay, the other sections take the newer text; where both
/// changed one, the editor comes back with the other version to pick.
async fn save(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Form(form): Form<SaveForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    if !handbook::is_page(&form.f) {
        return Err(AppError::NotFound);
    }
    let f = form.f.clone();
    let data = state.config.data_dir();
    let mine = form.text.replace("\r\n", "\n");
    let res = state
        .db
        .call(move |c| {
            let cur = handbook::read(c, &form.f)?;
            let (text, taken) = if form.base_rev == cur.rev {
                (mine, Vec::new())
            } else {
                let base = handbook::revision_text(c, &form.f, form.base_rev)?.unwrap_or_default();
                let m = crate::projectdoc::merge(&base, &cur.text, &mine);
                if !m.conflicts.is_empty() {
                    return Ok(Saved::Conflicts(cur, m));
                }
                (m.text, m.taken)
            };
            let what = match crate::projectdoc::changed_sections(&cur.text, &text) {
                w if w.is_empty() => "the whole file".to_owned(),
                w => w,
            };
            match handbook::save(c, &form.f, &cur.version(), &text, "owner", &what) {
                Ok(rev) => {
                    if let Err(e) = handbook::mirror(c, &data) {
                        tracing::warn!("writing out the handbook: {e:#}");
                    }
                    Ok(Saved::Done(rev, what, taken.join(", ")))
                }
                Err(handbook::SaveError::Unchanged) => Ok(Saved::Same),
                Err(e) => Err(anyhow::Error::from(e)),
            }
        })
        .await;
    match res {
        Ok(Saved::Done(rev, what, taken)) => {
            let e = Entry::new(&actor, Via::Page, Action::Write, format!("handbook: edited {f}, {what} (rev {rev})"))
                .ip(Some(ip.0));
            activity::write(&state.db, e).await;
            let taken = if taken.is_empty() { String::new() } else { format!("&taken={}", enc(&taken)) };
            Ok(Redirect::to(&format!("/handbook?f={}&saved={rev}{taken}", enc(&f))).into_response())
        }
        Ok(Saved::Same) => Ok(Redirect::to(&format!("/handbook?f={}&error=same", enc(&f))).into_response()),
        Ok(Saved::Conflicts(cur, m)) => {
            let names = state.db.call(|c| handbook::pages(c)).await.unwrap_or_default();
            let now = crate::util::now_ms();
            render(HandbookPage {
                nav: nav(&state, &actor, "/handbook").await.back_to("/handbook", "Handbook"),
                files: names
                    .into_iter()
                    .map(|n| FileRow { selected: n == cur.name, name: n, blurb: String::new() })
                    .collect(),
                picked: true,
                line: format!("rev {} · edited {} by {}", cur.rev, crate::util::ago(cur.updated_at, now), cur.author),
                sections: Vec::new(),
                rev: cur.rev,
                editor: Some(file_editor(
                    &cur.name,
                    m.text,
                    cur.rev,
                    MdEditor::conflicts_of(m.conflicts),
                    cur.author.clone(),
                )),
                file: Some(cur.name),
                history: Vec::new(),
                old: None,
                notice: None,
            })
        }
        Err(e) => {
            tracing::warn!("saving {f}: {e:#}");
            Ok(Redirect::to(&format!("/handbook?f={}&error=failed", enc(&f))).into_response())
        }
    }
}

#[derive(Deserialize)]
struct PreviewForm {
    text: String,
}

/// The editor's preview: the Markdown as the page shows it.
async fn preview(Extension(actor): Extension<Actor>, Form(f): Form<PreviewForm>) -> AppResult<Response> {
    actor.require_owner()?;
    let text: String = f.text.chars().take(handbook::MAX_FILE).collect();
    Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], local_links(&markdown::to_html(&text))).into_response())
}

#[derive(Deserialize)]
struct RevQuery {
    f: String,
    since: i64,
}

/// For the editor while it's open: a newer revision than the one it started from, by whom, and
/// which sections it changed.
async fn rev(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<RevQuery>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let v = state
        .db
        .call(move |c| {
            let cur = handbook::read(c, &q.f)?;
            let changed = if cur.rev > q.since {
                let base = handbook::revision_text(c, &q.f, q.since)?.unwrap_or_default();
                crate::projectdoc::changed_sections(&base, &cur.text)
            } else {
                String::new()
            };
            Ok(json!({"rev": cur.rev, "author": cur.author, "changed": changed}))
        })
        .await?;
    Ok(Json(v).into_response())
}

#[derive(Deserialize)]
struct BundleQuery {
    /// The version the machine has; nothing is sent when it's the current one.
    have: Option<String>,
}

/// Every file of the handbook, for `flow` to keep a copy on its machine (the owner, 2026-11-18,
/// D-209). Any signed-in caller may read it, as agents read it through MCP.
async fn api_bundle(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<BundleQuery>,
) -> AppResult<Response> {
    if matches!(actor, Actor::Anonymous | Actor::Builder { .. }) {
        return Err(AppError::Forbidden("the handbook is for signed-in callers"));
    }
    let (version, files) = state.db.call(|c| handbook::bundle(c)).await?;
    if q.have.as_deref() == Some(version.as_str()) {
        return Ok(Json(json!({"version": version, "unchanged": true})).into_response());
    }
    use base64::Engine;
    // Text as it is; an image's bytes in base64.
    let files: Vec<serde_json::Value> = files
        .into_iter()
        .map(|(p, bytes)| match String::from_utf8(bytes) {
            Ok(t) => json!({"path": p, "text": t}),
            Err(e) => json!({"path": p, "b64": base64::engine::general_purpose::STANDARD.encode(e.as_bytes())}),
        })
        .collect();
    Ok(Json(json!({"version": version, "files": files})).into_response())
}

#[cfg(test)]
mod tests {
    #[test]
    fn links_between_pages_open_here() {
        assert_eq!(
            super::local_links(
                r#"<a href="OWNER.md">x</a> <a href="https://x.y/a.md">y</a> <a href="MACHINE.md#ports">z</a>"#
            ),
            r#"<a href="/handbook?f=OWNER.md">x</a> <a href="https://x.y/a.md">y</a> <a href="/handbook?f=MACHINE.md#ports">z</a>"#
        );
    }
}
