//! The To test checklist: on the project's tab, on release pages and on build pages. The owner
//! ticks what they tried and works and sends; see `crate::totest`.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;
use axum::{Extension, Form, Router};
use rusqlite::Connection;
use serde_json::Value;

use super::{AppResult, AppState, ClientIp};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::records::{self, Kind, Query, Record};
use crate::util::ago;

pub fn routes() -> Router<AppState> {
    Router::new().route("/p/{slug}/to-test", post(send))
}

pub struct Item {
    pub id: i64,
    pub short: String,
    pub url: String,
    pub title: String,
    pub how: String,
    /// (class, text): "new in v0.0.4", "from v0.0.2" (yellow: waiting since an older release).
    pub chip: (&'static str, String),
    pub problems: Vec<String>,
}

pub struct Group {
    pub category: String,
    pub items: Vec<Item>,
}

pub struct View {
    pub slug: String,
    pub heading: String,
    pub sub: String,
    pub note: String,
    pub groups: Vec<Group>,
    /// Tested or dropped, newest first: (title, "Albums · tested 2 days ago, works").
    pub done: Vec<(String, String)>,
    pub done_total: i64,
    /// Where Send comes back to.
    pub back: String,
    pub is_owner: bool,
}

/// Where the list shows; it decides the heading and how items' releases read.
pub enum Place<'a> {
    Tab,
    Release(&'a Record),
    Build,
}

/// The newest release's label (its tag, else its title) for a project.
fn latest_release(c: &Connection, project_id: i64) -> anyhow::Result<Option<String>> {
    let mut list = records::query(
        c,
        &Query { project_id: Some(project_id), kinds: vec![Kind::Release], limit: 0, ..Default::default() },
    )?;
    list.sort_by_key(|r| r.created_at);
    Ok(list.last().map(|r| if r.str("tag").is_empty() { r.title.clone() } else { r.str("tag").to_owned() }))
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
        _ => latest_release(c, project_id)?,
    };
    let mut groups: Vec<Group> = Vec::new();
    for r in &items {
        let first = r.str("first_release");
        let chip = match (first, &current) {
            ("", _) => ("quiet", "new".to_owned()),
            (f, Some(cur)) if f == cur => ("quiet", format!("new in {f}")),
            (f, _) => ("warn", format!("from {f}")),
        };
        let problems = r
            .data
            .get("problems")
            .and_then(Value::as_array)
            .map(|ps| {
                ps.iter()
                    .filter_map(|p| {
                        let at = p.get("at").and_then(Value::as_i64).unwrap_or(0);
                        Some(format!("Problem ({}): {}", ago(at, now), p.get("note")?.as_str()?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let item = Item {
            id: r.id,
            short: r.short_id(),
            url: r.url(),
            title: r.title.clone(),
            how: r.body.clone(),
            chip,
            problems,
        };
        let cat = r.str("category").to_owned();
        match groups.iter_mut().find(|g| g.category == cat) {
            Some(g) => g.items.push(item),
            None => groups.push(Group { category: cat, items: vec![item] }),
        }
    }
    let older = items
        .iter()
        .filter(|r| !r.str("first_release").is_empty() && Some(r.str("first_release")) != current.as_deref())
        .count();
    let open = items.len();
    let (heading, sub, note) = match &place {
        Place::Tab => (
            "To test".to_owned(),
            format!("{open} open · the agent adds them as it builds; each new release and build carries them until tested"),
            String::new(),
        ),
        Place::Release(r) => (
            if r.str("tag").is_empty() { "To test in this release".to_owned() } else { format!("To test before tagging {}", r.str("tag")) },
            format!("{open} open: {} new, {older} from older releases", open - older),
            String::new(),
        ),
        Place::Build => (
            "To test in this build".to_owned(),
            format!("{open} open: what's new, and what's still untested from before"),
            "Everything not tested yet comes along with each new build. General feedback goes to the agent in its session.".to_owned(),
        ),
    };
    let done_items = crate::totest::done_items(c, project_id, 10)?;
    let done_total: i64 = c.query_row(
        "SELECT count(*) FROM records WHERE project_id = ?1 AND kind = 'test' AND state IN ('tested', 'dropped')",
        [project_id],
        |r| r.get(0),
    )?;
    let done = done_items
        .iter()
        .map(|r| {
            let how = if r.state == "dropped" { "dropped".to_owned() } else { "tested, works".to_owned() };
            (r.title.clone(), format!("{} · {how} {}", r.str("category"), ago(r.updated_at, now)))
        })
        .collect();
    Ok(View { slug: slug.to_owned(), heading, sub, note, groups, done, done_total, back, is_owner })
}

/// The owner sends what they ticked: tested, works.
async fn send(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path(slug): Path<String>,
    Form(f): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let ids: Vec<i64> = f.keys().filter_map(|k| k.strip_prefix("t").and_then(|n| n.parse().ok())).collect();
    let note = f.get("note").cloned().unwrap_or_default();
    let back = super::local_path(f.get("back").map(String::as_str)).unwrap_or_else(|| format!("/p/{slug}?tab=to-test"));
    let s = slug.clone();
    let done = state
        .db
        .call(move |c| {
            let p = crate::projects::require(c, &s)?;
            crate::totest::owner_tested(c, p.id, &ids, &note)
        })
        .await?;
    if !done.is_empty() {
        let what =
            format!("tested {}: {}", done.len(), done.iter().map(|r| r.short_id()).collect::<Vec<_>>().join(", "));
        activity::write(&state.db, Entry::new(&actor, Via::Page, Action::Write, what).project(slug).ip(Some(ip.0)))
            .await;
    }
    let sep = if back.contains('?') { '&' } else { '?' };
    Ok(Redirect::to(&format!("{back}{sep}tested={}", done.len())).into_response())
}
