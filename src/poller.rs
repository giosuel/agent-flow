//! Keeps PRs in step with GitHub: every few minutes, each active project's open PRs and every PR
//! AgentFlow still thinks is open. Merges, closes and CI failures become events for the project's
//! agent; a PR or release that becomes ready is a "something to review" notification.

use std::time::Duration;

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
        let mut round: u64 = 0;
        loop {
            tick.tick().await;
            if let Err(e) = poll_once(&state).await {
                tracing::warn!("GitHub poll: {e:#}");
            }
            if let Err(e) = refresh_guide_prs(&state).await {
                tracing::warn!("guides' PRs: {e:#}");
            }
            // Other people's repositories change slowly: every tenth round (30 minutes).
            if round.is_multiple_of(10)
                && let Err(e) = poll_contributions(&state).await
            {
                tracing::warn!("contribution watch: {e:#}");
            }
            round += 1;
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
    let infos = state.github.open_prs(&p.repo).await?;
    let listed: Vec<i64> = infos.iter().map(|i| i.number).collect();
    for info in infos {
        apply(state, p, &p.repo, info).await?;
    }
    // PRs still open here that GitHub no longer lists: fetch each from its own repository to learn
    // merged or closed. One that fails is logged and retried next round; the others go on.
    let pid = p.id;
    let known_open: Vec<(i64, String)> = state
        .db
        .call(move |c| {
            let open = records::query(
                c,
                &Query {
                    project_id: Some(pid),
                    kinds: vec![Kind::Pr],
                    states: vec!["open".into()],
                    ..Default::default()
                },
            )?;
            Ok(open.into_iter().map(|r| (r.number, r.str("repo").to_owned())).collect())
        })
        .await?;
    for (n, repo) in known_open.into_iter().filter(|(n, _)| !listed.contains(n)) {
        let repo = if repo.is_empty() { p.repo.clone() } else { repo };
        let fetched = state.github.pr(&repo, n).await;
        match fetched {
            Ok(info) => apply(state, p, &repo, info).await?,
            Err(e) => {
                let e = Entry::new(
                    &Actor::System("poller"),
                    Via::Poller,
                    Action::System,
                    format!("GitHub poll of {repo}#{n} failed: {e:#}"),
                )
                .project(p.slug.clone())
                .severity("warn");
                activity::write(&state.db, e).await;
            }
        }
    }
    Ok(())
}

/// Stores one PR's state (with its events) and tells the owner when it became mergeable.
pub async fn apply(state: &AppState, p: &projects::Project, repo: &str, info: PrInfo) -> anyhow::Result<()> {
    let (project, repo) = (p.clone(), repo.to_owned());
    let (r, res) = state
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            let out = prs::sync(&tx, &project, &repo, &info, prs::GITHUB)?;
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
                &Query { kinds: vec![Kind::Release], states: vec!["waiting".into()], limit: 0, ..Default::default() },
            )?;
            let mut out = Vec::new();
            for r in waiting {
                if r.data.get("notified_ready").and_then(Value::as_bool).unwrap_or(false) || !prs::release_ready(c, &r)?
                {
                    continue;
                }
                let mut data = serde_json::Map::new();
                data.insert("notified_ready".into(), true.into());
                let (r, _) =
                    records::update(c, &r, prs::GITHUB, records::Patch { data, quiet: true, ..Default::default() })?;
                let name = projects::get_by_id(c, r.project_id)?.name;
                records::event(
                    c,
                    Some(r.project_id),
                    Some(r.id),
                    "release.ready",
                    &format!("{} can be tagged: every PR is merged", r.str("tag")),
                    prs::GITHUB,
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

/// Checks every watched PR and issue on other people's repositories.
pub async fn poll_contributions(state: &AppState) -> anyhow::Result<()> {
    let list = state.db.call(|c| crate::contributions::watched(c)).await?;
    for r in list {
        let (repo, number) =
            (r.str("repo").to_owned(), r.data.get("number").and_then(serde_json::Value::as_i64).unwrap_or(0));
        match state.github.item(&repo, number).await {
            Ok(info) => {
                state.db.call(move |c| crate::contributions::update(c, &r, &info)).await?;
            }
            Err(e) => {
                let e = Entry::new(
                    &Actor::System("poller"),
                    Via::Poller,
                    Action::System,
                    format!("watching {repo}#{number} failed: {e:#}"),
                )
                .project(r.project.clone())
                .severity("warn");
                activity::write(&state.db, e).await;
            }
        }
    }
    Ok(())
}

/// The guides repository's open and recently merged PRs; a new one is something to review.
pub async fn refresh_guide_prs(state: &AppState) -> anyhow::Result<()> {
    let repo = state.config.handbook_repo.clone();
    let open = state.github.repo_prs(&repo, false).await?;
    let merged = state.github.repo_prs(&repo, true).await?;
    let new: Vec<crate::github::RepoPr> = {
        let mut cache = state.guide_prs.lock().unwrap();
        let new = if cache.fetched {
            open.iter().filter(|p| !cache.open.iter().any(|o| o.number == p.number)).cloned().collect()
        } else {
            Vec::new()
        };
        *cache = crate::web::GuidePrs { open, merged, fetched: true };
        new
    };
    for pr in new {
        state.push.spawn(
            push::Kind::Review,
            Notification {
                title: format!("Guides: #{} to review", pr.number),
                body: format!("{} ({})", pr.title, pr.files.join(", ")),
                url: "/guides".into(),
                tag: Some(format!("guides-{}", pr.number)),
            },
        );
    }
    Ok(())
}
