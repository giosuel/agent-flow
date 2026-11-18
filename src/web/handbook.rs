//! The Handbook page (once Guides): the handbook's pages by section, edited by the owner as a
//! section or a whole file; every save a revision, kept by AgentFlow and its backup, no git (the
//! owner, 2026-11-18: "lets actually make it so its not in git, we can just have it as actual
//! files, and then have the backup make sure we don't lose anything"). `GET /api/handbook` hands
//! every file to `flow`, which keeps a copy on each machine.

use askama::Template;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::pages::{Nav, nav, render};
use super::{AppError, AppResult, AppState, ClientIp};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::tz::Tz;
use crate::{handbook, markdown};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/handbook", get(page))
        .route("/handbook/save", post(save))
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
    html: String,
}

struct Editing {
    /// Section index, or None for the whole file.
    section: Option<usize>,
    label: String,
    text: String,
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
    version: String,
    sections: Vec<SectionView>,
    editing: Option<Editing>,
    history: Vec<RevRow>,
    /// An older revision being looked at: (rev, its sections).
    old: Option<(i64, Vec<SectionView>)>,
    rev: i64,
    notice: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct PageQuery {
    f: Option<String>,
    /// "file", or a section's index
    edit: Option<String>,
    history: Option<u8>,
    rev: Option<i64>,
    saved: Option<i64>,
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
            html: local_links(&markdown::to_html(without_heading(&s.text))),
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
    let editing = match (&file, q.edit.as_deref()) {
        (Some(g), Some("file")) => Some(Editing { section: None, label: g.name.clone(), text: g.text.clone() }),
        (Some(g), Some(i)) => i
            .parse::<usize>()
            .ok()
            .and_then(|i| g.sections.get(i).map(|s| (i, s)))
            .map(|(i, s)| Editing { section: Some(i), label: s.title.clone(), text: s.text.clone() }),
        _ => None,
    };
    let now = crate::util::now_ms();
    let error = q.error.as_deref().map(|e| match e {
        "stale" => "The file changed since you opened it (the consultant may have edited it). Nothing was saved; make your edit again.".to_owned(),
        "same" => "Nothing changed.".to_owned(),
        _ => "That didn't work; see the log.".to_owned(),
    });
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
        version: file.as_ref().map(|g| g.version()).unwrap_or_default(),
        sections: file.as_ref().map(section_views).unwrap_or_default(),
        rev: file.as_ref().map(|f| f.rev).unwrap_or(0),
        file: file.map(|f| f.name),
        editing,
        history: history
            .into_iter()
            .map(|r| RevRow { rev: r.rev, when: tz.short(r.at), who: r.author, what: r.what })
            .collect(),
        old,
        notice: q.saved.map(|r| format!("Saved as rev {r}. Agents read the new text from now on.")),
        error,
    })
}

#[derive(Deserialize)]
struct SaveForm {
    f: String,
    version: String,
    /// A section's index, or empty for the whole file.
    section: Option<String>,
    text: String,
}

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
    let result = state
        .db
        .call(move |c| {
            let g = handbook::read(c, &form.f)?;
            let (content, what) = match form.section.as_deref().filter(|s| !s.is_empty()) {
                Some(i) => {
                    let i: usize = i.parse().map_err(|_| anyhow::anyhow!("no such section"))?;
                    let title = g.sections.get(i).map(|s| s.title.clone()).unwrap_or_default();
                    (handbook::replace_section(&g, i, &form.text)?, format!("section {title}"))
                }
                None => (form.text.replace("\r\n", "\n"), "the whole file".into()),
            };
            let r = handbook::save(c, &form.f, &form.version, &content, "owner", &what);
            if r.is_ok()
                && let Err(e) = handbook::mirror(c, &data)
            {
                tracing::warn!("writing out the handbook: {e:#}");
            }
            Ok(r.map(|rev| (rev, what)))
        })
        .await?;
    match result {
        Ok((rev, what)) => {
            let e = Entry::new(&actor, Via::Page, Action::Write, format!("handbook: edited {f}, {what} (rev {rev})"))
                .ip(Some(ip.0));
            activity::write(&state.db, e).await;
            Ok(Redirect::to(&format!("/handbook?f={f}&saved={rev}")).into_response())
        }
        Err(e) => {
            let code = match e {
                handbook::SaveError::Stale => "stale",
                handbook::SaveError::Unchanged => "same",
                handbook::SaveError::Other(e) => {
                    tracing::warn!("saving {f}: {e:#}");
                    "failed"
                }
            };
            Ok(Redirect::to(&format!("/handbook?f={f}&error={code}")).into_response())
        }
    }
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
