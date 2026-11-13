//! The project document on the project's Notes tab, laid out like the Guides (the owner,
//! 2026-11-12: "make it look the same as the global notes"): a card per section with a pencil, the
//! whole document editable too, and its history. See `crate::projectdoc`.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;
use axum::{Extension, Form, Router};
use rusqlite::Connection;
use serde::Deserialize;

use super::{AppResult, AppState, ClientIp};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::projectdoc;
use crate::tz::Tz;

pub fn routes() -> Router<AppState> {
    Router::new().route("/p/{slug}/doc", post(save))
}

pub struct SectionView {
    pub index: usize,
    pub title: String,
    pub html: String,
}

pub struct Editing {
    /// The section's index, or None for the whole document.
    pub section: Option<usize>,
    pub label: String,
    pub text: String,
}

pub struct RevRow {
    pub rev: i64,
    pub when: String,
    pub who: String,
    pub what: String,
}

pub struct DocView {
    pub rev: i64,
    pub line: String,
    pub version: String,
    pub bytes: String,
    pub sections: Vec<SectionView>,
    pub editing: Option<Editing>,
    pub history: Vec<RevRow>,
    /// An older revision being looked at: (rev, its sections).
    pub old: Option<(i64, Vec<SectionView>)>,
    pub notice: Option<(bool, String)>,
}

/// What the Notes tab asks for: a section to edit ("2" or "all"), the history, an old revision.
pub struct Ask<'a> {
    pub edit: Option<&'a str>,
    pub history: bool,
    pub rev: Option<i64>,
    pub outcome: Option<&'a str>,
}

fn section_views(text: &str) -> Vec<SectionView> {
    projectdoc::sections(text)
        .into_iter()
        .enumerate()
        .map(|(index, (title, body))| {
            // The heading is the card's label; the rest is the card.
            let rest =
                if body.starts_with("## ") { body.split_once('\n').map(|x| x.1).unwrap_or("") } else { body.as_str() };
            SectionView { index, title, html: crate::markdown::to_html(rest) }
        })
        .collect()
}

pub fn view(c: &Connection, project_id: i64, ask: &Ask, tz: &Tz, now: i64) -> anyhow::Result<DocView> {
    let d = projectdoc::get(c, project_id)?;
    let parts = projectdoc::sections(&d.text);
    let editing = match ask.edit {
        Some("all") => Some(Editing { section: None, label: "the whole document".into(), text: d.text.clone() }),
        Some(i) => i.parse::<usize>().ok().and_then(|i| {
            parts.get(i).map(|(t, body)| Editing {
                section: Some(i),
                label: t.clone(),
                text: body.trim_end().to_owned(),
            })
        }),
        None => None,
    };
    let history = if ask.history {
        projectdoc::revisions(c, project_id, 50)?
            .into_iter()
            .map(|r| RevRow { rev: r.rev, when: tz.short(r.at), who: r.author, what: r.what })
            .collect()
    } else {
        Vec::new()
    };
    let old = match ask.rev {
        Some(r) if r > 0 && r < d.rev => projectdoc::revision_text(c, project_id, r)?.map(|t| (r, section_views(&t))),
        _ => None,
    };
    let notice = ask.outcome.map(|o| match o {
        "saved" => (true, "Saved. The agent reads it at the start of its next session.".to_owned()),
        "same" => (true, "Nothing changed.".to_owned()),
        "stale" => (false, "The document changed while you were editing (the agent may have edited it). Nothing was saved; here it is as it is now.".to_owned()),
        "big" => (false, format!("That would make it bigger than {} KB, which is read at the start of every session. Nothing was saved.", projectdoc::MAX_BYTES / 1024)),
        _ => (false, "That didn't work; nothing was saved.".to_owned()),
    });
    Ok(DocView {
        rev: d.rev,
        line: if d.rev == 0 {
            String::new()
        } else {
            format!("rev {} · edited {} by {} ({})", d.rev, crate::util::ago(d.updated_at, now), d.author, d.what)
        },
        version: d.version(),
        bytes: format!("{:.1} KB of {} KB", d.text.len() as f64 / 1024.0, projectdoc::MAX_BYTES / 1024),
        sections: section_views(&d.text),
        editing,
        history,
        old,
        notice,
    })
}

#[derive(Deserialize)]
struct SaveForm {
    #[serde(default)]
    section: String,
    version: String,
    text: String,
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
    let res = state
        .db
        .call(move |c| {
            let Some(p) = crate::projects::get(c, &s)? else { anyhow::bail!("no project") };
            let old = projectdoc::get(c, p.id)?;
            if old.version() != f.version {
                return Ok(("stale", p.id, String::new()));
            }
            let (text, what) = match f.section.trim().parse::<usize>() {
                Ok(i) => {
                    let parts = projectdoc::sections(&old.text);
                    let Some((title, _)) = parts.get(i) else { anyhow::bail!("no such section") };
                    // The owner's text replaces the section as written, heading and all.
                    let mut out = String::new();
                    for (j, (_, body)) in parts.iter().enumerate() {
                        if j == i {
                            let t = f.text.trim_end();
                            if !t.is_empty() {
                                out.push_str(t);
                                out.push_str("\n\n");
                            }
                        } else {
                            out.push_str(body);
                        }
                    }
                    (out, format!("section {title}"))
                }
                Err(_) => (f.text.clone(), "the whole document".into()),
            };
            match projectdoc::set(c, p.id, &text, "owner", &what) {
                Ok((_, true)) => Ok(("saved", p.id, what)),
                Ok((_, false)) => Ok(("same", p.id, what)),
                Err(e) if e.to_string().contains("KB") => Ok(("big", p.id, what)),
                Err(e) => Err(e),
            }
        })
        .await;
    let (code, what) = match res {
        Ok((code, _, what)) => (code, what),
        Err(_) => ("error", String::new()),
    };
    if code == "saved" {
        let e = Entry::new(&actor, Via::Page, Action::Write, format!("project document of {slug}: {what}"))
            .project(slug.clone())
            .ip(Some(ip.0));
        activity::write(&state.db, e).await;
    }
    let back = if matches!(code, "big") { "&edit=all" } else { "" };
    Ok(Redirect::to(&format!("/p/{slug}?tab=notes&doc={code}{back}")).into_response())
}
