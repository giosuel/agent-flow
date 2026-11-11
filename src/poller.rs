//! Keeps PRs in step with GitHub: every few minutes, each active project's open PRs and every PR
//! AgentFlow still thinks is open. Merges, closes and CI failures become events for the project's
//! agent; a PR or release that becomes ready is a "something to review" notification.

use std::time::Duration;

use rusqlite::Connection;
use serde_json::Value;

use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::github::PrInfo;
use crate::push::{self, Notification};
use crate::records::{self, Kind, Query};
use crate::web::AppState;
use crate::{projects, prs};

pub const EVERY: Duration = Duration::from_secs(180);

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = poll_once(&state).await {
                tracing::warn!("GitHub poll: {e:#}");
            }
        }
    });
}

/// One round over every active project with a repository.
pub async fn poll_once(state: &AppState) -> anyhow::Result<()> {
    let list = state.db.call(|c| projects::list(c)).await?;
    for p in list.into_iter().filter(|p| p.status == "active" && !p.repo.is_empty()) {
        if let Err(e) = poll_project(state, &p).await {
            let e = Entry::new(
                &Actor::System("poller"),
                Via::Poller,
                Action::System,
                format!("GitHub poll of {} failed: {e:#}", p.repo),
            )
            .project(p.slug.clone())
            .severity("warn");
            activity::write(&state.db, e).await;
        }
    }
    check_releases(state).await
}

pub async fn poll_project(state: &AppState, p: &projects::Project) -> anyhow::Result<()> {
    let mut infos = state.github.open_prs(&p.repo).await?;
    // PRs we still have as open but GitHub no longer lists: fetch them to learn merged or closed.
    let pid = p.id;
    let known_open: Vec<i64> = state
        .db
        .call(move |c| {
            Ok(records::query(
                c,
                &Query {
                    project_id: Some(pid),
                    kinds: vec![Kind::Pr],
                    states: vec!["open".into()],
                    limit: 500,
                    ..Default::default()
                },
            )?
            .into_iter()
            .map(|r| r.number)
            .collect())
        })
        .await?;
    for n in known_open {
        if !infos.iter().any(|i| i.number == n) {
            infos.push(state.github.pr(&p.repo, n).await?);
        }
    }
    for info in infos {
        apply(state, p, info).await?;
    }
    Ok(())
}

/// Stores one PR's state and reacts to what changed.
pub async fn apply(state: &AppState, p: &projects::Project, info: PrInfo) -> anyhow::Result<()> {
    let project = p.clone();
    let (r, res) = state
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            let out = prs::sync(&tx, &project, &project.repo, &info, "poller")?;
            let (r, res) = &out;
            if res.merged {
                records::event(
                    &tx,
                    Some(project.id),
                    Some(r.id),
                    "pr.merged",
                    &format!("{} merged: {}", r.short_id(), r.title),
                    "github",
                )?;
            }
            if res.closed {
                records::event(
                    &tx,
                    Some(project.id),
                    Some(r.id),
                    "pr.closed",
                    &format!("{} closed without merging", r.short_id()),
                    "github",
                )?;
            }
            if res.ci_failed {
                records::event(
                    &tx,
                    Some(project.id),
                    Some(r.id),
                    "pr.ci_failed",
                    &format!("CI failing on {}", r.short_id()),
                    "github",
                )?;
            }
            tx.commit()?;
            Ok(out)
        })
        .await?;
    if res.became_mergeable {
        notify_ready(state, &p.name, &r);
    }
    Ok(())
}

pub fn notify_ready(state: &AppState, project_name: &str, pr: &records::Record) {
    state.push.spawn(
        push::Kind::Review,
        Notification {
            title: format!("{project_name}: ready to merge"),
            body: format!("{} {}", pr.short_id(), pr.title),
            url: format!("/p/{}?tab=prs", pr.project),
            tag: Some(pr.ref_id.clone()),
        },
    );
}

/// Releases whose PRs are all merged now: tell the owner once.
async fn check_releases(state: &AppState) -> anyhow::Result<()> {
    let ready = state
        .db
        .call(|c| {
            let waiting = records::query(
                c,
                &Query { kinds: vec![Kind::Release], states: vec!["waiting".into()], limit: 200, ..Default::default() },
            )?;
            let mut out = Vec::new();
            for r in waiting {
                if r.data.get("notified_ready").and_then(Value::as_bool).unwrap_or(false) || !release_ready(c, &r)? {
                    continue;
                }
                let mut data = serde_json::Map::new();
                data.insert("notified_ready".into(), true.into());
                let (r, _) =
                    records::update(c, &r, "poller", records::Patch { data, quiet: true, ..Default::default() })?;
                let name = projects::get_by_id(c, r.project_id)?.name;
                records::event(
                    c,
                    Some(r.project_id),
                    Some(r.id),
                    "release.ready",
                    &format!("{} can be tagged: every PR is merged", r.str("tag")),
                    "github",
                )?;
                out.push((name, r));
            }
            Ok(out)
        })
        .await?;
    for (name, r) in ready {
        state.push.spawn(
            push::Kind::Review,
            Notification {
                title: format!("{name}: ready to tag"),
                body: format!("{} {}", r.str("tag"), r.title),
                url: r.url(),
                tag: Some(r.ref_id.clone()),
            },
        );
    }
    Ok(())
}

/// Every PR the release waits for is merged.
pub fn release_ready(c: &Connection, r: &records::Record) -> anyhow::Result<bool> {
    let waits: Vec<String> = r
        .data
        .get("waits_for")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    for w in waits {
        if records::find_str(c, &w, None)?.state != "merged" {
            return Ok(false);
        }
    }
    Ok(true)
}
