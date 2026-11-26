//! What the owner has to try: on the Builds tab, on release pages and on build pages. The owner
//! marks each item Works, Problem (a note to the agent; the item stays open) or Skip (the owner,
//! 2026-11-23, Q-8: built as drawn on S-40); see `crate::totest`.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;
use axum::{Extension, Form, Router};
use rusqlite::Connection;
use serde_json::Value;

use super::{AppError, AppResult, AppState, ClientIp};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::records::{self, Kind, Query, Record};
use crate::util::ago;

pub fn routes() -> Router<AppState> {
    Router::new().route("/p/{slug}/{id}/try", post(mark))
}

/// One thing to try, as a row with its verdict buttons.
pub struct Item {
    pub short: String,
    pub url: String,
    pub title: String,
    pub how: String,
    /// (tone, text): "in B-66" (the build it came with, when that isn't the newest), "from
    /// v0.0.2" (yellow: waiting since an older release), "new in v0.0.4" (on a release's page).
    pub chip: Option<(&'static str, String)>,
    /// The problems found so far: "2 px lower than the status bar (you, 21 Nov)".
    pub problems: Vec<String>,
    pub open: bool,
    /// For one that's done: "works · 2 days ago", "skipped · …", "dropped · …".
    pub outcome: String,
    pub category: String,
}

/// The newest release's label (its tag, else its title) for a project.
pub fn latest_release(c: &Connection, project_id: i64) -> anyhow::Result<Option<String>> {
    let mut list = records::query(
        c,
        &Query { project_id: Some(project_id), kinds: vec![Kind::Release], limit: 0, ..Default::default() },
    )?;
    list.sort_by_key(|r| r.created_at);
    Ok(list.last().map(|r| if r.str("tag").is_empty() { r.title.clone() } else { r.str("tag").to_owned() }))
}

/// The project's builds that are still offered, oldest first.
pub fn builds_oldest_first(c: &Connection, project_id: i64) -> anyhow::Result<Vec<Record>> {
    let mut list = records::query(
        c,
        &Query { project_id: Some(project_id), kinds: vec![Kind::Build], limit: 0, ..Default::default() },
    )?;
    list.retain(|b| b.state != "withdrawn");
    list.sort_by_key(|b| (b.created_at, b.number));
    Ok(list)
}

/// Where an item reads from: the newest build (no chip for its own items), the current release
/// ("new in v0.4" on a release's page), or neither.
pub struct Context<'a> {
    pub builds: &'a [&'a Record],
    pub newest: Option<&'a str>,
    pub release: Option<&'a str>,
    /// On a release's page its own items say "new in …".
    pub release_page: bool,
    /// On the Builds tab, a build of several platforms (one commit) goes by one record, its
    /// primary: each record's short id to that one's.
    pub primary: Option<&'a HashMap<String, String>>,
    pub now: i64,
}

pub fn item(r: &Record, cx: &Context<'_>) -> Item {
    let build = crate::totest::build_of(r, cx.builds).map(|b| cx.primary.and_then(|p| p.get(&b)).cloned().unwrap_or(b));
    let first = r.str("first_release");
    let chip = if !first.is_empty() && Some(first) != cx.release {
        Some((super::tone::tone("waiting"), format!("from {first}")))
    } else if cx.release_page && !first.is_empty() {
        Some((super::tone::tone("new"), format!("new in {first}")))
    } else {
        match &build {
            Some(b) if Some(b.as_str()) != cx.newest => Some(("info", format!("in {b}"))),
            _ => None,
        }
    };
    let problems = r
        .data
        .get("problems")
        .and_then(Value::as_array)
        .map(|ps| {
            ps.iter()
                .filter_map(|p| {
                    let at = p.get("at").and_then(Value::as_i64).unwrap_or(0);
                    let by = if p.get("by").and_then(Value::as_str) == Some("owner") { "you, " } else { "" };
                    Some(format!("{} ({by}{})", p.get("note")?.as_str()?, ago(at, cx.now)))
                })
                .collect()
        })
        .unwrap_or_default();
    let outcome = match r.state.as_str() {
        "open" => String::new(),
        "tested" => format!("works · {}", ago(r.updated_at, cx.now)),
        _ if r.str("dropped_because") == "skipped by the owner" => format!("skipped · {}", ago(r.updated_at, cx.now)),
        _ => format!("dropped · {}", ago(r.updated_at, cx.now)),
    };
    Item {
        short: r.short_id(),
        url: r.url(),
        title: r.title.clone(),
        how: r.body.clone(),
        chip,
        problems,
        open: r.state == "open",
        outcome,
        category: r.str("category").to_owned(),
    }
}

pub struct Group {
    pub category: String,
    pub items: Vec<Item>,
}

/// Items by category, in the order categories first appear.
pub fn grouped(items: Vec<Item>) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for i in items {
        match groups.iter_mut().find(|g| g.category == i.category) {
            Some(g) => g.items.push(i),
            None => groups.push(Group { category: i.category.clone(), items: vec![i] }),
        }
    }
    groups
}

/// The list on a release's or a build's page.
pub struct View {
    pub slug: String,
    pub heading: String,
    pub sub: String,
    pub note: String,
    pub groups: Vec<Group>,
    /// Where a verdict comes back to.
    pub back: String,
    pub is_owner: bool,
}

/// Where the list shows; it decides the heading and how items' releases read.
pub enum Place<'a> {
    Release(&'a Record),
    Build,
}

pub fn view(
    c: &Connection,
    project_id: i64,
    slug: &str,
    place: Place<'_>,
    back: String,
    is_owner: bool,
    now: i64,
) -> anyhow::Result<View> {
    let items = crate::totest::open_items(c, project_id)?;
    let current = match &place {
        Place::Release(r) => Some(if r.str("tag").is_empty() { r.title.clone() } else { r.str("tag").to_owned() }),
        Place::Build => latest_release(c, project_id)?,
    };
    let builds = builds_oldest_first(c, project_id)?;
    let refs: Vec<&Record> = builds.iter().collect();
    let cx = Context {
        builds: &refs,
        newest: None,
        release: current.as_deref(),
        release_page: matches!(place, Place::Release(_)),
        primary: None,
        now,
    };
    let older = items
        .iter()
        .filter(|r| !r.str("first_release").is_empty() && Some(r.str("first_release")) != current.as_deref())
        .count();
    let open = items.len();
    let groups = grouped(items.iter().map(|r| item(r, &cx)).collect());
    let (heading, sub, note) = match &place {
        Place::Release(r) => (
            if r.str("tag").is_empty() { "To test in this release".to_owned() } else { format!("To test before tagging {}", r.str("tag")) },
            format!("{open} open: {} new, {older} from older releases", open - older),
            String::new(),
        ),
        Place::Build => (
            "To test in this build".to_owned(),
            format!("{open} open: what's new, and what's still untested from before"),
            "Everything not tried yet comes along with each new build. Works or Skip takes an item off the list; Problem tells the agent and keeps it.".to_owned(),
        ),
    };
    Ok(View { slug: slug.to_owned(), heading, sub, note, groups, back, is_owner })
}

/// The owner's verdict on one item: works, problem (with a note) or skip.
async fn mark(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path((slug, id)): Path<(String, String)>,
    Form(f): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let verdict = f.get("v").cloned().unwrap_or_default();
    let note = f.get("note").cloned().unwrap_or_default();
    if note.chars().count() > 2000 {
        return Err(AppError::BadRequest("keep the note under 2,000 characters".into()));
    }
    let back = super::local_path(f.get("back").map(String::as_str)).unwrap_or_else(|| format!("/p/{slug}?tab=builds"));
    let (s, i, v) = (slug.clone(), id.clone(), verdict.clone());
    let done = state
        .db
        .call(move |c| {
            let r = records::find_str(c, &format!("{s}/{i}"), None)?;
            crate::totest::owner_mark(c, &r, &v, &note)
        })
        .await
        .map_err(|e| AppError::BadRequest(format!("{e:#}")))?;
    let what = format!("marked {} {verdict}", done.short_id());
    activity::write(
        &state.db,
        Entry::new(&actor, Via::Page, Action::Write, what).project(slug).target(done.ref_id.clone()).ip(Some(ip.0)),
    )
    .await;
    let (path, frag) =
        back.split_once('#').map(|(a, b)| (a.to_owned(), format!("#{b}"))).unwrap_or((back.clone(), String::new()));
    let sep = if path.contains('?') { '&' } else { '?' };
    Ok(Redirect::to(&format!("{path}{sep}tried={}.{verdict}{frag}", done.short_id())).into_response())
}

/// The line shown after a verdict, from the redirect's `tried=TT-5.works`.
pub fn tried_notice(tried: &str) -> Option<String> {
    let (id, verdict) = tried.split_once('.')?;
    if id.len() > 20 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    Some(match verdict {
        "works" => format!("{id} works: off the list. The agent hears it."),
        "problem" => format!("{id}: the agent hears the problem in your words. It stays to try."),
        "skip" => format!("{id} skipped: off the list. The agent hears it."),
        _ => return None,
    })
}
