//! The Log page: everything that happened, filtered like a cloud console's log viewer (the owner,
//! 2026-11-07: "like logs in gcp or azure"), with a timeline, filters on the side, entries that
//! open into all their fields, and a live stream.

use std::convert::Infallible;
use std::time::Duration;

use askama::Template;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use serde::Deserialize;

use super::pages::{Nav, nav, render};
use super::{AppResult, AppState};
use crate::auth::Actor;
use crate::logview::{self, Filter, LogRow};
use crate::tz::Tz;
use crate::util::now_ms;

pub fn routes() -> Router<AppState> {
    Router::new().route("/log", get(page)).route("/log/stream", get(stream))
}

const PAGE: i64 = 100;
const BUCKETS: i64 = 48;
const RANGES: &[(&str, &str, i64)] = &[
    ("1h", "1 h", 3_600_000),
    ("24h", "24 h", 86_400_000),
    ("7d", "7 d", 7 * 86_400_000),
    ("30d", "30 d", 30 * 86_400_000),
    ("all", "All", 0),
];

#[derive(Deserialize, Default)]
struct LogQuery {
    #[serde(default)]
    q: String,
    range: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    before: Option<i64>,
}

struct Facet {
    title: &'static str,
    values: Vec<(String, i64, String, bool)>,
}

struct Bar {
    height: i64,
    href: String,
    label: String,
}

struct EntryView {
    seq: i64,
    time: String,
    actor: String,
    action: String,
    summary: String,
    severity: String,
    fields: Vec<(String, String, Option<String>)>,
    record_url: Option<String>,
    around: String,
    archived: bool,
}

#[derive(Template)]
#[template(path = "log.html")]
struct LogPage {
    nav: Nav,
    tz: String,
    q: String,
    range: String,
    ranges: Vec<(&'static str, &'static str, bool)>,
    span: String,
    facets: Vec<Facet>,
    bars: Vec<Bar>,
    axis: Vec<String>,
    total: i64,
    entries: Vec<EntryView>,
    older: Option<String>,
    latest: i64,
    archive: Vec<(String, u64)>,
    reaches_archive: bool,
}

fn url(q: &str, range: &str, from: Option<i64>, to: Option<i64>, before: Option<i64>) -> String {
    let mut parts = vec![format!("q={}", enc(q))];
    match (from, to) {
        (Some(f), Some(t)) => parts.push(format!("from={f}&to={t}")),
        _ => parts.push(format!("range={range}")),
    }
    if let Some(b) = before {
        parts.push(format!("before={b}"));
    }
    format!("/log?{}", parts.join("&"))
}

pub(super) fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => (b as char).to_string(),
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The query with `field:value` set (replacing that field), for the facet links.
fn with_field(q: &str, field: &str, value: &str) -> String {
    let mut parts: Vec<&str> = q
        .split_whitespace()
        .filter(|t| !t.trim_start_matches('-').to_lowercase().starts_with(&format!("{field}:")))
        .collect();
    let token = format!("{field}:{value}");
    parts.push(&token);
    parts.join(" ")
}

/// A record's page from a log target like `vinculum/Q-12`.
fn record_url(target: &str) -> Option<String> {
    let r = crate::records::RecordRef::parse(target).ok()?;
    let p = r.project?;
    crate::projects::check_slug(&p).ok()?;
    Some(format!("/p/{p}/{}-{}", r.kind.prefix(), r.number))
}

fn entry(r: &LogRow, archived: bool, q: &str, tz: &Tz) -> EntryView {
    let opt = |v: &Option<String>| v.clone().unwrap_or_default();
    let mut fields = vec![
        ("seq".to_owned(), r.seq.to_string(), None),
        ("time".into(), format!("{} ({})", tz.full(r.at), crate::util::fmt_utc(r.at)), None),
        ("severity".into(), r.severity.clone(), Some(with_field(q, "severity", &r.severity))),
        ("actor".into(), format!("{} ({})", r.actor, r.actor_kind), Some(with_field(q, "actor", &r.actor))),
        ("via".into(), r.via.clone(), Some(with_field(q, "via", &r.via))),
        ("action".into(), r.action.clone(), Some(with_field(q, "action", &r.action))),
    ];
    if let Some(p) = &r.project {
        fields.push(("project".into(), p.clone(), Some(with_field(q, "project", p))));
    }
    if let Some(t) = &r.target {
        fields.push(("target".into(), t.clone(), Some(with_field(q, "target", t))));
    }
    if let Some(s) = r.status {
        fields.push(("status".into(), s.to_string(), None));
    }
    if let Some(d) = r.duration_ms {
        fields.push(("duration".into(), format!("{d} ms"), None));
    }
    if r.ip.is_some() {
        fields.push(("ip".into(), opt(&r.ip), Some(with_field(q, "ip", &opt(&r.ip)))));
    }
    if let Some(d) = &r.detail {
        fields.push(("detail".into(), d.clone(), None));
    }
    EntryView {
        seq: r.seq,
        time: tz.full(r.at),
        actor: r.actor.clone(),
        action: r.action.clone(),
        summary: r.summary.clone(),
        severity: r.severity.clone(),
        fields: fields.into_iter().map(|(k, v, link)| (k, v, link.map(|l| url(&l, "24h", None, None, None)))).collect(),
        record_url: r.target.as_deref().and_then(record_url),
        around: url("", "", Some(r.at - 5 * 60_000), Some(r.at + 5 * 60_000), None),
        archived,
    }
}

async fn page(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(lq): Query<LogQuery>,
    tz: Tz,
) -> AppResult<Response> {
    actor.require_owner()?;
    let now = now_ms();
    let range = lq.range.clone().filter(|r| RANGES.iter().any(|x| x.0 == r)).unwrap_or_else(|| "24h".into());
    let mut filter = Filter::parse(&lq.q);
    match (lq.from, lq.to) {
        (Some(f), Some(t)) if f < t => {
            filter.from = Some(f);
            filter.to = Some(t);
        }
        _ => {
            let span = RANGES.iter().find(|x| x.0 == range).map(|x| x.2).unwrap_or(0);
            if span > 0 {
                filter.from = Some(now - span);
            }
        }
    }
    let f = filter.clone();
    let before = lq.before;
    let (rows, facets, bars_raw, first_at, latest) = state
        .db
        .call(move |c| {
            let rows = logview::page(c, &f, before, PAGE)?;
            let mut facets = Vec::new();
            for (title, col) in [
                ("Severity", "severity"),
                ("Project", "project"),
                ("Actor", "actor"),
                ("Action", "action"),
                ("Via", "via"),
            ] {
                facets.push((title, col, logview::facet(c, &f, col)?));
            }
            let first: i64 = c.query_row("SELECT coalesce(min(at), ?1) FROM log", [now], |r| r.get(0))?;
            let from = f.from.unwrap_or(first);
            let to = f.to.unwrap_or(now + 1);
            let bars = (from, to, logview::histogram(c, &f, from, to, BUCKETS)?);
            Ok((rows, facets, bars, first, logview::latest_seq(c)?))
        })
        .await?;

    // Archived reads, when the range reaches back past what the database still holds.
    let cutoff = now - logview::READS_KEPT_DAYS * 86_400_000;
    let reaches_archive = filter.from.is_none_or(|f| f < cutoff);
    let mut entries: Vec<EntryView> = rows.iter().map(|r| entry(r, false, &lq.q, &tz)).collect();
    if reaches_archive && (rows.len() as i64) < PAGE {
        let data = state.config.data_dir();
        let (f, b, need) = (filter.clone(), rows.last().map(|r| r.seq).or(before), (PAGE as usize) - rows.len());
        let archived = tokio::task::spawn_blocking(move || logview::search_archive(&data, &f, b, need))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        entries.extend(archived.iter().map(|r| entry(r, true, &lq.q, &tz)));
    }
    let older =
        (entries.len() as i64 >= PAGE).then(|| url(&lq.q, &range, lq.from, lq.to, entries.last().map(|e| e.seq)));

    let (from, to, counts) = bars_raw;
    let max = counts.iter().copied().max().unwrap_or(0).max(1);
    let width = ((to - from) / BUCKETS).max(1);
    let bars = counts
        .iter()
        .enumerate()
        .map(|(i, &n)| {
            let b_from = from + i as i64 * width;
            Bar {
                height: if n == 0 { 0 } else { (n * 100 / max).max(4) },
                href: url(&lq.q, &range, Some(b_from), Some(b_from + width), None),
                label: format!("{n} between {} and {}", tz.short(b_from), tz.short(b_from + width)),
            }
        })
        .collect();
    let axis = (0..=4).map(|i| tz.short(from + (to - from) * i / 4)).collect();
    let total = counts.iter().sum();
    let archive = logview::archive_months(&state.config.data_dir());
    let _ = first_at;
    render(LogPage {
        nav: nav(&state, &actor, "/log").await,
        tz: tz.name().to_owned(),
        span: match (lq.from, lq.to) {
            (Some(f), Some(t)) => format!("{} – {}", tz.short(f), tz.short(t)),
            _ => RANGES.iter().find(|x| x.0 == range).map(|x| x.1.to_owned()).unwrap_or_default(),
        },
        ranges: RANGES.iter().map(|(k, l, _)| (*k, *l, *k == range && lq.from.is_none())).collect(),
        facets: facets
            .into_iter()
            .map(|(title, col, values)| Facet {
                title,
                values: values
                    .into_iter()
                    .filter(|(v, _)| !v.is_empty())
                    .map(|(v, n)| {
                        let active = filter.fields.iter().any(|(c, x, neg)| c == col && *x == v.to_lowercase() && !neg);
                        let link = url(
                            &with_field(&lq.q, if col == "actor_kind" { "kind" } else { col }, &v),
                            &range,
                            lq.from,
                            lq.to,
                            None,
                        );
                        (v, n, link, active)
                    })
                    .collect(),
            })
            .collect(),
        q: lq.q,
        range,
        bars,
        axis,
        total,
        entries,
        older,
        latest,
        archive,
        reaches_archive,
    })
}

#[derive(Deserialize)]
struct StreamQuery {
    #[serde(default)]
    q: String,
    after: i64,
}

/// New entries as they're written (server-sent events), for "Stream".
async fn stream(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(sq): Query<StreamQuery>,
    tz: Tz,
) -> Response {
    if actor.require_owner().is_err() {
        return super::json_error(axum::http::StatusCode::FORBIDDEN, "owner only");
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);
    let filter = Filter::parse(&sq.q);
    let mut after = sq.after;
    tokio::spawn(async move {
        loop {
            let f = filter.clone();
            let rows = state.db.call(move |c| logview::newer(c, &f, after)).await.unwrap_or_default();
            for r in &rows {
                after = after.max(r.seq);
                let data = serde_json::json!({
                    "seq": r.seq, "time": tz.full(r.at), "actor": r.actor, "action": r.action,
                    "summary": r.summary, "severity": r.severity, "target": r.target,
                });
                if tx.send(Ok(Event::default().data(data.to_string()))).await.is_err() {
                    return;
                }
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx)).keep_alive(KeepAlive::default()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facet_links_replace_their_field() {
        assert_eq!(with_field("severity:info merge", "severity", "warn"), "merge severity:warn");
        assert_eq!(with_field("-action:read", "action", "write"), "action:write");
        assert_eq!(record_url("vinculum/Q-12").as_deref(), Some("/p/vinculum/Q-12"));
        assert_eq!(record_url("../x/Q-1"), None);
        assert_eq!(enc("a b&c"), "a+b%26c");
    }
}
