//! The Dashboard: one status line, one featured question to answer right there, every other
//! waiting item in one grouped list, then latest activity and the projects. On the phone it's the
//! home-screen app's start page, with the same grouping (the owner, 2026-11-11).

use askama::Template;
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Router};
use serde::Deserialize;
use serde_json::Value;

use super::icons::icon;
use super::pages::{Nav, nav, render};
use super::{AppResult, AppState};
use crate::auth::Actor;
use crate::records::{self, Kind, Record};
use crate::util::{ago, now_ms};
use crate::{machine, markdown, projects, waiting};

pub fn routes() -> Router<AppState> {
    Router::new().route("/", get(dashboard))
}

struct Featured {
    id: String,
    short: String,
    slug: String,
    url: String,
    project: String,
    when: String,
    /// What it blocks: (title, page).
    blocks: Vec<(String, String)>,
    title: String,
    body_html: String,
    options: Vec<(usize, String, String, bool)>,
}

struct Item {
    icon: &'static str,
    title: String,
    sub: String,
    url: String,
    action: &'static str,
}

struct Group {
    title: &'static str,
    items: Vec<Item>,
}

struct Activity {
    time: String,
    who: String,
    what: String,
    color: &'static str,
    url: Option<String>,
}

struct ProjectLine {
    slug: String,
    name: String,
    online: bool,
    milestone: String,
    percent: i64,
    waiting: i64,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardPage {
    nav: Nav,
    online: usize,
    agents: usize,
    machine: String,
    warnings: i64,
    today: String,
    notice: Option<String>,
    featured: Option<Featured>,
    groups: Vec<Group>,
    merge_count: usize,
    activity: Vec<Activity>,
    projects: Vec<ProjectLine>,
}

#[derive(Deserialize)]
struct DashQuery {
    /// Set after answering from here: the question that was answered.
    answered: Option<String>,
}

async fn dashboard(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<DashQuery>,
) -> AppResult<Response> {
    let now = now_ms();
    let answered = q.answered.clone();
    // The guides' open pull requests, from the poller's cache.
    let repo = state.config.handbook_repo.clone();
    let guide_prs: Vec<Item> = state
        .guide_prs
        .lock()
        .unwrap()
        .open
        .iter()
        .map(|p| Item {
            icon: icon("doc"),
            title: p.title.clone(),
            sub: format!("{} · {repo}/pull/{}", p.files.join(", "), p.number),
            url: format!("{}/files", p.url),
            action: "View on GitHub",
        })
        .collect();
    let data = state
        .db
        .call(move |c| {
            let w = waiting::load(c)?;
            let all_projects = projects::list(c)?;
            let names: std::collections::HashMap<String, String> =
                all_projects.iter().map(|p| (p.slug.clone(), p.name.clone())).collect();
            let name = |slug: &str| names.get(slug).cloned().unwrap_or_else(|| slug.to_owned());

            let featured = match w.questions.first() {
                Some(q) => Some(featured(q, name(&q.project), waiting::blocks(c, q)?, now)),
                None => None,
            };
            let mut groups = Vec::new();
            let more: Vec<Item> = w
                .questions
                .iter()
                .skip(1)
                .map(|q| {
                    let blocks = waiting::blocks(c, q).unwrap_or_default();
                    let detail = if blocks.is_empty() {
                        format!("asked {}", ago(q.created_at, now))
                    } else {
                        format!("blocks {}", blocks.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join(", "))
                    };
                    Item {
                        icon: icon("question"),
                        title: q.title.clone(),
                        sub: format!("{} · {detail}", name(&q.project)),
                        url: q.url(),
                        action: "Answer",
                    }
                })
                .collect();
            if !more.is_empty() {
                groups.push(Group {
                    title: if featured.is_some() { "More questions" } else { "Questions" },
                    items: more,
                });
            }
            let merges: Vec<Item> = w
                .merges
                .iter()
                .map(|r| Item {
                    icon: icon(if r.kind == Kind::Release { "tag" } else { "merge" }),
                    title: r.title.clone(),
                    sub: if r.kind == Kind::Release {
                        format!("{} · {} · every PR merged", name(&r.project), r.str("tag"))
                    } else {
                        format!("{} · #{} · ready, CI {}", name(&r.project), r.number, r.str("ci"))
                    },
                    url: r.url(),
                    action: if r.kind == Kind::Release { "Tag" } else { "Merge" },
                })
                .collect();
            let merge_count = merges.len();
            if !merges.is_empty() {
                groups.push(Group { title: "Merge and release", items: merges });
            }
            let builds: Vec<Item> = w
                .builds
                .iter()
                .map(|r| Item {
                    icon: icon("build"),
                    title: r.title.clone(),
                    sub: format!("{} · {}", name(&r.project), ago(r.created_at, now)),
                    url: r.url(),
                    action: "Try it",
                })
                .collect();
            if !builds.is_empty() {
                groups.push(Group { title: "Builds to try", items: builds });
            }
            if !guide_prs.is_empty() {
                groups.push(Group { title: "Guides to review", items: guide_prs });
            }

            let activity = records::recent_events(c, None, 30)?
                .into_iter()
                // A button being added is part of the PR's story, not news of its own.
                .filter(|e| e.kind != "action.added")
                .take(12)
                .map(|e| Activity {
                    time: crate::util::fmt_utc(e.at).get(11..16).unwrap_or("").into(),
                    who: match e.actor.as_str() {
                        "owner" => "you".into(),
                        a => names.get(a).cloned().unwrap_or_else(|| a.to_owned()),
                    },
                    color: if e.kind == "question.answered" || e.actor == "owner" {
                        "var(--accent)"
                    } else if e.kind.contains("failed") || e.kind == "owner.notified" {
                        "var(--warn)"
                    } else {
                        "var(--info)"
                    },
                    url: e.record.as_ref().and_then(|r| r.split_once('/')).map(|(p, id)| format!("/p/{p}/{id}")),
                    what: lowercase_first(&e.summary),
                })
                .collect();

            let mut lines = Vec::new();
            let open_by_project = |id: i64| w.questions.iter().filter(|q| q.project_id == id).count() as i64;
            for p in all_projects.iter().filter(|p| p.status == "active") {
                let ms = projects::milestones(c, p.id)?;
                let current = ms.iter().filter(|m| m.parent_id.is_none()).find(|m| m.state == "active");
                let percent = match current {
                    Some(m) => {
                        let ids: Vec<i64> = projects::with_steps(&ms, m.id);
                        let tasks = records::query(
                            c,
                            &records::Query {
                                project_id: Some(p.id),
                                kinds: vec![Kind::Task],
                                milestone_ids: ids,
                                limit: 0,
                                ..Default::default()
                            },
                        )?;
                        let done = tasks.iter().filter(|t| t.state == "done").count();
                        if tasks.is_empty() { 0 } else { (done * 100 / tasks.len()) as i64 }
                    }
                    None => 0,
                };
                lines.push(ProjectLine {
                    slug: p.slug.clone(),
                    name: p.name.clone(),
                    online: p.agent.online(now),
                    milestone: current
                        .map(|m| format!("{} · {}", m.name, m.title))
                        .unwrap_or_else(|| "no active milestone".into()),
                    percent,
                    waiting: open_by_project(p.id),
                });
            }
            let agents = all_projects.iter().filter(|p| p.status == "active" && p.agent.seen_at.is_some()).count();
            let online = all_projects.iter().filter(|p| p.agent.online(now)).count();
            let warnings: i64 = c.query_row(
                "SELECT count(*) FROM log WHERE severity IN ('warn', 'error') AND at > ?1",
                [now - 86_400_000],
                |r| r.get(0),
            )?;
            let notice = match answered {
                Some(id) => match records::RecordRef::parse(&id) {
                    Ok(r) => records::try_find(c, &r, None)?
                        .filter(|q| q.state == "answered")
                        .map(|q| format!("Sent to {}: {}", name(&q.project), q.ref_id)),
                    Err(_) => None,
                },
                None => None,
            };
            Ok((featured, groups, merge_count, activity, lines, agents, online, warnings, notice))
        })
        .await?;
    let (featured, groups, merge_count, activity, projects, agents, online, warnings, notice) = data;
    let today = time::OffsetDateTime::now_utc();
    let f = time::macros::format_description!("[weekday] [day padding:none] [month repr:long] · [hour]:[minute] UTC");
    render(DashboardPage {
        nav: nav(&state, &actor, "/").await,
        online,
        agents,
        machine: machine::snapshot().summary(),
        warnings,
        today: today.format(&f).unwrap_or_default(),
        notice,
        featured,
        groups,
        merge_count,
        activity,
        projects,
    })
}

fn featured(q: &Record, project: String, blocks: Vec<(String, String)>, now: i64) -> Featured {
    let rec = q.data.get("recommended").and_then(Value::as_u64).unwrap_or(0) as usize;
    let options = q
        .data
        .get("options")
        .and_then(Value::as_array)
        .map(|opts| {
            opts.iter()
                .enumerate()
                .map(|(i, o)| {
                    let s = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
                    (i + 1, s("title"), s("detail"), rec == i + 1)
                })
                .collect()
        })
        .unwrap_or_default();
    Featured {
        id: q.ref_id.clone(),
        short: q.short_id(),
        slug: q.project.clone(),
        url: q.url(),
        project,
        when: format!("asked {}", ago(q.created_at, now)),
        blocks,
        title: q.title.clone(),
        body_html: markdown::to_html(&q.body),
        options,
    }
}

fn lowercase_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        // Keep ids and names ("Q-1 …", "Vinculum …") as they are; lower only plain words.
        Some(f)
            if f.is_uppercase() && c.clone().next().is_some_and(|n| n.is_lowercase()) && !s.starts_with("Owner") =>
        {
            f.to_lowercase().chain(c).collect()
        }
        _ => s.replace("Owner answered", "answered"),
    }
}
