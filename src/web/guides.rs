//! The Guides page: the handbook's files by section, the owner's edits (committed locally), and
//! the consultant's pull requests with a Merge button. Diffs stay on GitHub (the owner,
//! 2026-11-10: "lets actually outsource any diffing to github, and not reinvent the wheel").

use askama::Template;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Router};
use serde::Deserialize;

use super::pages::{Nav, nav, render};
use super::{AppError, AppResult, AppState, ClientIp};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::{guides, markdown};

pub fn routes() -> Router<AppState> {
    Router::new().route("/guides", get(page)).route("/guides/save", post(save)).route("/guides/prs/merge", post(merge))
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

struct PrRow {
    number: i64,
    title: String,
    url: String,
    files: String,
    changes: String,
    by: String,
    head: String,
}

struct Editing {
    /// Section index, or None for the whole file.
    section: Option<usize>,
    label: String,
    text: String,
}

#[derive(Template)]
#[template(path = "guides.html")]
struct GuidesPage {
    nav: Nav,
    repo: String,
    files: Vec<FileRow>,
    open: Vec<PrRow>,
    merged: Vec<PrRow>,
    file: Option<String>,
    /// The pull requests are what's shown (on the phone, instead of the list of files).
    prs_view: bool,
    version: String,
    sections: Vec<SectionView>,
    editing: Option<Editing>,
    /// Open PRs that change the file being edited.
    clash: Vec<i64>,
    notice: Option<String>,
    error: Option<String>,
    unpushed: usize,
}

#[derive(Deserialize)]
struct PageQuery {
    f: Option<String>,
    /// "file", or a section's index
    edit: Option<String>,
    saved: Option<String>,
    merged: Option<i64>,
    error: Option<String>,
    /// "prs": the pull requests as their own screen on the phone (the owner, 2026-11-11:
    /// "clicking on pull requests in guides is broken on mobile").
    view: Option<String>,
}

/// The first plain line of a guide, for the file list, with Markdown marks and links' targets removed.
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

fn pr_row(p: &crate::github::RepoPr) -> PrRow {
    PrRow {
        number: p.number,
        title: p.title.clone(),
        url: p.url.clone(),
        files: p.files.join(", "),
        changes: format!("+{} −{}", p.additions, p.deletions),
        by: p.author.clone(),
        head: p.head_ref_oid.clone(),
    }
}

async fn page(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<PageQuery>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let dir = state.config.handbook_dir();
    if !crate::util::lock(&state.guide_prs).fetched {
        // First visit since start: fetch now rather than show nothing, but don't let an
        // unreachable GitHub hold up the page (the poller keeps trying).
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), crate::poller::refresh_guide_prs(&state)).await;
    }
    let prs = crate::util::lock(&state.guide_prs).clone();
    let file = q.f.clone().filter(|f| guides::check_name(f).is_ok());
    // After a merge, or a merge that failed, the pull requests are what you were looking at.
    let prs_view = file.is_none() && (q.view.as_deref() == Some("prs") || q.merged.is_some() || q.error.is_some());
    let (names, guide) = {
        let dir = dir.clone();
        let file = file.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let names = guides::list(&dir)?;
            let blurbs: Vec<(String, String)> = names
                .iter()
                .map(|n| (n.clone(), guides::read(&dir, n).map(|g| blurb(&g.text)).unwrap_or_default()))
                .collect();
            let guide = match &file {
                Some(f) => Some(guides::read(&dir, f)?),
                None => None,
            };
            Ok((blurbs, guide))
        })
        .await
        .map_err(|e| AppError::Internal(e.into()))??
    };
    let editing = match (&guide, q.edit.as_deref()) {
        (Some(g), Some("file")) => Some(Editing { section: None, label: g.name.clone(), text: g.text.clone() }),
        (Some(g), Some(i)) => i
            .parse::<usize>()
            .ok()
            .and_then(|i| g.sections.get(i).map(|s| (i, s)))
            .map(|(i, s)| Editing { section: Some(i), label: s.title.clone(), text: s.text.clone() }),
        _ => None,
    };
    // Editing a file an open PR also changes: one of the two will need redoing after the other.
    let clash: Vec<i64> = match (&file, &editing) {
        (Some(f), Some(_)) => prs.open.iter().filter(|p| p.files.iter().any(|x| x == f)).map(|p| p.number).collect(),
        _ => Vec::new(),
    };
    let repo = guides::Repo { dir: dir.clone(), author: state.config.git_author.clone() };
    let unpushed = tokio::task::spawn_blocking(move || repo.unpushed()).await.unwrap_or(0);
    let notice = match (&q.saved, q.merged) {
        (Some(c), _) => {
            Some(format!("Saved and committed as {c} on main, not pushed. Push from the checkout when you're ready."))
        }
        (None, Some(n)) => Some(format!("Merged #{n}; the checkout is up to date, so agents read the new text.")),
        _ => None,
    };
    let error = q.error.as_deref().map(|e| match e {
        "stale" => "The file changed since you opened it. Nothing was saved; make your edit again.".to_owned(),
        "branch" => "The handbook checkout isn't on main. Nothing was saved.".to_owned(),
        "dirty" => "The file has uncommitted changes in the checkout. Nothing was saved.".to_owned(),
        "same" => "Nothing changed.".to_owned(),
        "pull" => "Merged on GitHub, but updating the checkout failed (does it have unpushed edits? push them first, then pull).".to_owned(),
        _ => "That didn't work; see the log.".to_owned(),
    });
    let page_nav = nav(&state, &actor, "/guides").await;
    render(GuidesPage {
        nav: if file.is_some() || prs_view { page_nav.back_to("/guides", "Guides") } else { page_nav },
        prs_view,
        repo: state.config.handbook_repo.clone(),
        files: names
            .into_iter()
            .map(|(n, b)| FileRow { selected: file.as_deref() == Some(n.as_str()), name: n, blurb: b })
            .collect(),
        open: prs.open.iter().map(pr_row).collect(),
        merged: prs.merged.iter().map(pr_row).collect(),
        version: guide.as_ref().map(|g| g.version()).unwrap_or_default(),
        sections: guide
            .as_ref()
            .map(|g| {
                g.sections
                    .iter()
                    .enumerate()
                    .map(|(i, s)| SectionView {
                        index: i,
                        title: s.title.clone(),
                        html: markdown::to_html(without_heading(&s.text)),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        file,
        editing,
        clash,
        notice,
        error,
        unpushed,
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
    guides::check_name(&form.f).map_err(|_| AppError::NotFound)?;
    let dir = state.config.handbook_dir();
    let repo = guides::Repo { dir: dir.clone(), author: state.config.git_author.clone() };
    let f = form.f.clone();
    let _checkout = state.handbook.lock().await;
    let result = tokio::task::spawn_blocking(move || -> Result<(String, String), guides::SaveError> {
        let g = guides::read(&dir, &form.f)?;
        let (content, what) = match form.section.as_deref().filter(|s| !s.is_empty()) {
            Some(i) => {
                let i: usize = i.parse().map_err(|_| anyhow::anyhow!("no such section"))?;
                let title = g.sections.get(i).map(|s| s.title.clone()).unwrap_or_default();
                (guides::replace_section(&g, i, &form.text)?, title)
            }
            None => (form.text.replace("\r\n", "\n"), "the whole file".into()),
        };
        let commit = repo.save(&form.f, &form.version, &content, &what)?;
        Ok((commit, what))
    })
    .await
    .map_err(|e| AppError::Internal(e.into()))?;
    match result {
        Ok((commit, what)) => {
            let e = Entry::new(&actor, Via::Page, Action::Write, format!("edited {f}: {what} ({commit}, not pushed)"))
                .ip(Some(ip.0));
            activity::write(&state.db, e).await;
            Ok(Redirect::to(&format!("/guides?f={f}&saved={commit}")).into_response())
        }
        Err(e) => {
            use guides::SaveError::*;
            let code = match e {
                Stale => "stale",
                NotOnMain => "branch",
                Dirty => "dirty",
                Unchanged => "same",
                Other(e) => {
                    tracing::warn!("saving {f}: {e:#}");
                    "failed"
                }
            };
            Ok(Redirect::to(&format!("/guides?f={f}&error={code}")).into_response())
        }
    }
}

#[derive(Deserialize)]
struct MergeForm {
    number: i64,
    /// The commit the page showed; GitHub refuses the merge if the branch has moved since.
    head: String,
}

/// Merges one of the guides' PRs on GitHub, then brings the checkout up to date so agents read it.
async fn merge(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Form(form): Form<MergeForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    if !crate::util::lock(&state.guide_prs).open.iter().any(|p| p.number == form.number) {
        return Err(AppError::BadRequest(format!("#{} isn't an open pull request of the guides", form.number)));
    }
    if !state.config.github_writes {
        return Err(AppError::BadRequest("This instance doesn't write to GitHub (github_writes = false).".into()));
    }
    let repo = state.config.handbook_repo.clone();
    let out = state.github.merge(&repo, form.number, &form.head).await.map_err(|e| {
        let msg = format!("{e:#}");
        if msg.contains("Head branch was modified") || msg.contains("head commit") {
            AppError::BadRequest(format!(
                "#{} changed since you looked at it; nothing was merged. Reload the page and review it again.",
                form.number
            ))
        } else {
            AppError::BadRequest(msg)
        }
    })?;
    let e = Entry::new(
        &actor,
        Via::Button,
        Action::Run,
        format!("merged {repo}#{}: {}", form.number, crate::activity::clip(&out, 200)),
    )
    .ip(Some(ip.0));
    activity::write(&state.db, e).await;
    // Fast-forward only: the owner's own unpushed edits are never merged or rebased behind their back.
    let dir = state.config.handbook_dir();
    let mut argv = guides::git_argv(&dir);
    argv.extend(["pull", "--ff-only", "-q"].map(String::from));
    let pulled = {
        let _checkout = state.handbook.lock().await;
        crate::hands::run_async::<crate::hands::ExecOut>(crate::hands::Job::Exec { argv, cwd: None, timeout_s: 120 })
            .await
    };
    let _ = crate::poller::refresh_guide_prs(&state).await;
    match pulled {
        Ok(o) if o.success() => Ok(Redirect::to(&format!("/guides?view=prs&merged={}", form.number)).into_response()),
        Ok(o) => {
            tracing::warn!("pulling the handbook after #{}: {}", form.number, o.stderr.trim());
            Ok(Redirect::to("/guides?view=prs&error=pull").into_response())
        }
        Err(e) => {
            tracing::warn!("pulling the handbook after #{}: {e}", form.number);
            Ok(Redirect::to("/guides?view=prs&error=pull").into_response())
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn blurbs_lose_markdown() {
        let b = super::blurb("# MACHINE\n\nThe server. Read [OWNER.md](OWNER.md) **first**.\n");
        assert_eq!(b, "The server. Read OWNER.md first.");
    }
}
