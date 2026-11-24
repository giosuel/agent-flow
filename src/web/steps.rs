//! A step's page (it opens in the record dialog): its state, the agent's summary once it's
//! concluded, and its tasks in order (the owner, 2026-11-12).

use askama::Template;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Router};

use super::pages::{Nav, nav, render};
use super::{AppError, AppResult, AppState};
use crate::auth::Actor;
use crate::markdown;
use crate::projects;
use crate::records::Record;
use crate::tz::Tz;

pub fn routes() -> Router<AppState> {
    Router::new().route("/p/{slug}/steps/{id}", get(step))
}

pub struct Row {
    pub n: usize,
    pub short: String,
    pub url: String,
    pub title: String,
    pub state: String,
    pub est: String,
    pub took: String,
    /// Took more than estimated (by more than a tenth), less, or about right.
    pub over: bool,
    pub under: bool,
    pub added: String,
    pub importance: String,
    pub skip_reason: String,
}

pub struct ReviewView {
    pub reviewers: Vec<String>,
    pub commit: String,
    pub owner_words: String,
    /// (text, url): a note or a link; url empty when it's neither.
    pub reports: Vec<(String, String)>,
    pub counts: String,
}

pub struct Link {
    pub short: String,
    pub url: String,
    pub title: String,
    pub sub: String,
}

#[derive(Template)]
#[template(path = "step.html")]
struct StepPage {
    nav: Nav,
    project_name: String,
    project_slug: String,
    name: String,
    milestone: String,
    title: String,
    state: &'static str,
    when: String,
    summary_html: String,
    description: String,
    rows: Vec<Row>,
    est_total: String,
    took_total: String,
    added_count: usize,
    biggest_miss: String,
    timings: Vec<crate::times::TimingRow>,
    decisions: Vec<Link>,
    follow_ups: Vec<Link>,
    with_owner: Vec<Link>,
    review: Option<ReviewView>,
}

async fn step(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((slug, id)): Path<(String, i64)>,
    tz: Tz,
) -> AppResult<Response> {
    let page = state
        .db
        .call(move |c| {
            let Some(p) = projects::get(c, &slug)? else { return Ok(None) };
            let Ok(m) = projects::get_milestone(c, id) else { return Ok(None) };
            if m.project_id != p.id {
                return Ok(None);
            }
            let parent = match m.parent_id {
                Some(pid) => projects::get_milestone(c, pid)?.name,
                None => String::new(),
            };
            let tasks: Vec<Record> = crate::steps::tasks_of(c, m.id)?;
            let now = crate::util::now_ms();
            let (times, _) = crate::times::step_times(c, m.id, now)?;
            let timings = crate::times::of_step(c, m.id)?;
            let from = m.started_at.unwrap_or(i64::MAX);
            let to = m.closed_at.unwrap_or(now);
            let all = crate::records::query(c, &crate::records::Query { project_id: Some(p.id), limit: 0, ..Default::default() })?;
            let decisions: Vec<Link> = all
                .iter()
                .filter(|r| r.kind == crate::records::Kind::Decision && r.state != "archived")
                .filter(|r| {
                    let at = r.data.get("said_at").and_then(serde_json::Value::as_i64).unwrap_or(r.created_at);
                    at >= from && at <= to
                })
                .map(|r| Link { short: r.short_id(), url: r.url(), title: r.title.clone(), sub: r.str("owner_words").chars().take(140).collect() })
                .collect();
            let names: Vec<String> = [m.name.to_lowercase(), format!("{}/{}", parent.to_lowercase(), m.name.to_lowercase())].into();
            let follow_ups: Vec<Link> = all
                .iter()
                .filter(|r| r.kind == crate::records::Kind::Task && r.milestone_id != Some(m.id))
                .filter(|r| names.contains(&r.str("source_ref").trim().to_lowercase()))
                .map(|r| Link { short: r.short_id(), url: r.url(), title: r.title.clone(), sub: format!("{} · {}", r.milestone.clone().unwrap_or_default(), r.state) })
                .collect();
            // Still with the owner: the step's PRs not merged yet, and what's open to test from its time.
            let ids: Vec<i64> = tasks.iter().map(|t| t.id).collect();
            let mut with_owner = Vec::new();
            let mut stmt = c.prepare(
                "SELECT DISTINCT l.from_id FROM links l JOIN records r ON r.id = l.from_id
                 WHERE l.relation = 'implements' AND r.kind = 'pr' AND r.state = 'open' AND l.to_id IN (SELECT value FROM json_each(?1))",
            )?;
            let prs: Vec<i64> = stmt.query_map([serde_json::to_string(&ids)?], |r| r.get(0))?.collect::<Result<_, _>>()?;
            for id in prs {
                let pr = crate::records::get(c, id)?;
                with_owner.push(Link { short: format!("#{}", pr.number), url: pr.url(), title: pr.title.clone(), sub: "to merge".into() });
            }
            let tests = all
                .iter()
                .filter(|r| r.kind == crate::records::Kind::Test && r.state == "open" && r.created_at >= from && r.created_at <= to)
                .count();
            if tests > 0 {
                with_owner.push(Link { short: String::new(), url: format!("/p/{}?tab=to-test", p.slug), title: format!("{tests} item{} to test", if tests == 1 { "" } else { "s" }), sub: String::new() });
            }
            let review = if m.review.is_empty() {
                None
            } else {
                let r: serde_json::Value = serde_json::from_str(&m.review).unwrap_or_default();
                let strs = |k: &str| -> Vec<String> {
                    r[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default()
                };
                let reports = strs("reports")
                    .into_iter()
                    .map(|x| {
                        if x.starts_with("https://") {
                            return (x.clone(), x);
                        }
                        let url = crate::records::RecordRef::parse(&x)
                            .ok()
                            .and_then(|rr| crate::records::try_find(c, &rr, Some(&p.slug)).ok().flatten())
                            .filter(|rec| rec.project_id == p.id)
                            .map(|rec| (format!("{} {}", rec.short_id(), rec.title), rec.url()));
                        url.unwrap_or((x, String::new()))
                    })
                    .collect();
                let n = |st: &str| tasks.iter().filter(|t| t.state == st).count();
                let open = tasks.iter().filter(|t| crate::steps::OPEN.contains(&t.state.as_str())).count();
                Some(ReviewView {
                    reviewers: strs("reviewers"),
                    commit: r["commit"].as_str().unwrap_or("").to_owned(),
                    owner_words: r["owner_words"].as_str().unwrap_or("").to_owned(),
                    reports,
                    counts: format!(
                        "{} findings · {} done, {} open, {} parked, {} skipped",
                        tasks.len(),
                        n("done") + n("review"),
                        open,
                        n("parked"),
                        n("skipped")
                    ),
                })
            };
            Ok(Some((p, m, parent, tasks, (times, timings, decisions, follow_ups, with_owner, review))))
        })
        .await?
        .ok_or(AppError::NotFound)?;
    let (p, m, parent, mut tasks, (times, timings, decisions, follow_ups, with_owner, review)) = page;
    // A review's findings by importance (high first), then in the agent's order.
    if review.is_some() {
        let rank = |t: &Record| match t.str("importance") {
            "high" => 0,
            "medium" => 1,
            "low" => 2,
            _ => 3,
        };
        tasks.sort_by_key(rank);
    }
    let state_word = match m.state.as_str() {
        "closed" => "concluded",
        "active" => "active",
        _ => "planned",
    };
    let when = match (m.started_at, m.closed_at) {
        (Some(a), Some(b)) => format!("{} – {}", tz.short(a), tz.short(b)),
        (Some(a), None) => format!("started {}", tz.short(a)),
        (None, Some(b)) => format!("concluded {}", tz.short(b)),
        (None, None) => "not started yet".into(),
    };
    let mut est_total = 0;
    let mut miss: Option<(i64, String)> = None;
    let rows: Vec<Row> = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let est = crate::times::estimate_min(t);
            let took = times.share_ms.get(&t.id).copied().unwrap_or(0);
            if t.state != "skipped" {
                est_total += est;
            }
            let diff = took - est * 60_000;
            if est > 0 && t.state == "done" && miss.as_ref().is_none_or(|m| diff.abs() > m.0.abs()) {
                let word = if diff >= 0 { "over" } else { "under" };
                miss = Some((diff, format!("{}, {} {word}", t.short_id(), crate::times::fmt_ms(diff.abs()))));
            }
            let added = if t.data.get("added").and_then(serde_json::Value::as_bool).unwrap_or(false) {
                format!("added · {}", super::projects::source_label(t.str("source")))
            } else {
                String::new()
            };
            Row {
                n: i + 1,
                short: t.short_id(),
                url: t.url(),
                title: t.title.clone(),
                state: t.state.clone(),
                est: if est > 0 { crate::times::fmt_min(est) } else { t.str("estimate").to_owned() },
                took: if t.state == "skipped" { String::new() } else { crate::times::fmt_ms(took) },
                over: est > 0 && took > est * 66_000,
                under: est > 0 && t.state == "done" && took < est * 54_000,
                added,
                importance: t.str("importance").to_owned(),
                skip_reason: t.str("skip_reason").to_owned(),
            }
        })
        .collect();
    let added_count = rows.iter().filter(|r| !r.added.is_empty()).count();
    render(StepPage {
        nav: nav(&state, &actor, "/projects").await.back_to(format!("/p/{}?tab=tasks", p.slug), &p.name),
        project_name: p.name.clone(),
        project_slug: p.slug.clone(),
        name: m.name.clone(),
        milestone: parent,
        title: m.title.clone(),
        state: state_word,
        when,
        summary_html: markdown::to_html(&m.summary),
        description: m.description.clone(),
        rows,
        est_total: crate::times::fmt_min(est_total),
        took_total: crate::times::fmt_ms(times.total_ms),
        added_count,
        biggest_miss: miss.filter(|m| m.0.abs() >= 10 * 60_000).map(|m| m.1).unwrap_or_default(),
        timings: crate::times::rows(&timings, &tz),
        decisions,
        follow_ups,
        with_owner,
        review,
    })
}
