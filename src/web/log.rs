//! The Log page (the owner, 2026-11-17, from the mockups on the canvas's "Log" page: "the new log
//! window is a masterpiece"): categories as the main filter, project, actor and severity as
//! dropdowns, the histogram and an aligned table in one panel. A build job is one row whose output
//! folds open; identical entries in the same second fold into "×N". It streams live by default
//! ("please make stream the default option … and then the button is pause") and loads older
//! entries as you scroll ("like an infinite scroller, no manual button").

use std::convert::Infallible;
use std::time::Duration;

use askama::Template;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use serde::Deserialize;

use super::pages::{Nav, nav, render};
use super::{AppResult, AppState};
use crate::auth::Actor;
use crate::logview::{self, CATEGORIES, Filter, LogRow};
use crate::tz::Tz;
use crate::util::now_ms;

pub fn routes() -> Router<AppState> {
    Router::new().route("/log", get(page)).route("/log/rows", get(rows)).route("/log/stream", get(stream))
}

const PAGE: i64 = 100;
const BUCKETS: i64 = 60;
const RANGES: &[(&str, &str, i64)] = &[
    ("1h", "1 h", 3_600_000),
    ("24h", "24 h", 86_400_000),
    ("7d", "7 d", 7 * 86_400_000),
    ("30d", "30 d", 30 * 86_400_000),
    ("all", "All", 0),
];
/// Output lines a job's row shows before "N more lines".
const JOB_LINES: usize = 5;

#[derive(Deserialize, Default, Clone)]
struct LogQuery {
    #[serde(default)]
    q: String,
    range: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    before: Option<i64>,
}

/// The end of the list: the range it covered, and a link to a longer one.
type End = Option<(String, Option<(String, String)>)>;

/// A category tab.
struct Tab {
    label: &'static str,
    icon: &'static str,
    count: i64,
    href: String,
    on: bool,
}

/// A dropdown: its label, what's picked, and (value, count, link, on).
struct Drop {
    label: &'static str,
    picked: String,
    all_href: String,
    values: Vec<(String, i64, String, bool)>,
}

struct Bar {
    height: i64,
    href: String,
    label: String,
}

/// One row of the table.
pub struct Row {
    seq: i64,
    time: String,
    cat: &'static str,
    cat_label: &'static str,
    cat_icon: &'static str,
    actor: String,
    project: String,
    summary: String,
    severity: String,
    /// Identical entries folded into this one.
    count: usize,
    archived: bool,
    /// Details: (key, value).
    fields: Vec<(String, String)>,
    /// Filter and open: (label, link, icon).
    actions: Vec<(String, String, &'static str)>,
    /// The query that finds this entry's kind, to copy.
    query: String,
    /// A build job's end: its outcome and its first lines (class, text), and how many more.
    job_outcome: Option<String>,
    job_ok: bool,
    lines: Vec<(&'static str, String)>,
    more_lines: usize,
}

#[derive(Template)]
#[template(path = "log.html")]
struct LogPage {
    nav: Nav,
    tz: String,
    q: String,
    range: String,
    ranges: Vec<(&'static str, &'static str, String, bool)>,
    span: String,
    tabs: Vec<Tab>,
    drops: Vec<Drop>,
    bars: Vec<Bar>,
    axis: Vec<String>,
    total: i64,
    rows: Vec<Row>,
    next: Option<String>,
    end: End,
    latest: i64,
    live: bool,
    archive: Vec<(String, u64)>,
    reset: Option<String>,
}

#[derive(Template)]
#[template(path = "log_rows.html")]
struct RowsPart {
    rows: Vec<Row>,
    next: Option<String>,
    end: End,
}

/// Build output lines are entries of their own; the list shows a job as one row (its end, with
/// its lines), unless the query asks for them.
fn parse_query(q: &str) -> Filter {
    let mut f = Filter::parse(q);
    let about_output = f.fields.iter().any(|(c, _, _)| matches!(c.as_str(), "action" | "target"));
    if !about_output {
        f.fields.push(("action".into(), "output".into(), true));
    }
    f
}

fn url(path: &str, q: &str, range: &str, from: Option<i64>, to: Option<i64>, before: Option<i64>) -> String {
    let mut parts = Vec::new();
    if !q.is_empty() {
        parts.push(format!("q={}", enc(q)));
    }
    match (from, to) {
        (Some(f), Some(t)) => parts.push(format!("from={f}&to={t}")),
        _ => parts.push(format!("range={range}")),
    }
    if let Some(b) = before {
        parts.push(format!("before={b}"));
    }
    format!("{path}?{}", parts.join("&"))
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

/// The query with `field:value` in place of any earlier `field:…`; an empty value removes it.
fn with_field(q: &str, field: &str, value: &str) -> String {
    let mut parts: Vec<String> = q
        .split_whitespace()
        .filter(|t| !t.trim_start_matches('-').to_lowercase().starts_with(&format!("{field}:")))
        .map(str::to_owned)
        .collect();
    if !value.is_empty() {
        let v = if value.contains(' ') { format!("\"{value}\"") } else { value.to_owned() };
        parts.push(format!("{field}:{v}"));
    }
    parts.join(" ")
}

/// The value the query gives a field, if any (not negated).
fn field_of(q: &str, field: &str) -> Option<String> {
    q.split_whitespace().find_map(|t| t.strip_prefix(&format!("{field}:")).map(|v| v.trim_matches('"').to_owned()))
}

fn record_url(target: &str) -> Option<String> {
    let r = crate::records::RecordRef::parse(target).ok()?;
    let p = r.project?;
    crate::projects::check_slug(&p).ok()?;
    Some(format!("/p/{p}/{}-{}", r.kind.prefix(), r.number))
}

fn cat_meta(cat: &str) -> (&'static str, &'static str, &'static str) {
    let label = CATEGORIES.iter().find(|(k, _)| *k == cat).map(|(_, l)| *l).unwrap_or("System");
    let (key, icon) = match cat {
        "agents" => ("agents", "robot"),
        "api" => ("api", "code"),
        "builds" => ("builds", "hammer"),
        "conversations" => ("conversations", "message"),
        "you" => ("you", "person"),
        _ => ("system", "gear"),
    };
    (key, if label == "Builds & jobs" { "Builds" } else { label }, icon)
}

fn row(r: &LogRow, archived: bool, range: &str, tz: &Tz) -> Row {
    let (cat, cat_label, cat_icon) = cat_meta(logview::category(&r.via, &r.actor_kind));
    let filter = |field: &str, value: &str| url("/log", &with_field("", field, value), range, None, None, None);
    let mut fields = vec![("When".to_owned(), format!("{} ({})", tz.full(r.at), crate::util::fmt_utc(r.at)))];
    fields.push(("Actor".into(), format!("{} ({})", r.actor, r.actor_kind)));
    fields.push(("How".into(), format!("{} · {}", r.via, r.action)));
    if let Some(t) = &r.target {
        fields.push(("Target".into(), t.clone()));
    }
    if let Some(s) = r.status {
        fields.push((
            "Status".into(),
            match r.duration_ms {
                Some(d) => format!("{s} · {d} ms"),
                None => s.to_string(),
            },
        ));
    }
    if let Some(ip) = &r.ip {
        fields.push(("From".into(), ip.clone()));
    }
    let mut actions: Vec<(String, String, &'static str)> = Vec::new();
    // A build job's end carries its last lines.
    let job: Option<(i64, Vec<String>)> = (r.via == "build")
        .then(|| r.detail.as_deref().and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok()))
        .flatten()
        .and_then(|d| {
            let id = d.get("job")?.as_i64()?;
            let lines = d.get("lines")?.as_array()?.iter().filter_map(|l| l.as_str().map(str::to_owned)).collect();
            Some((id, lines))
        });
    if let Some((id, _)) = &job {
        actions.push(("Open job page".into(), format!("/machine/jobs/{id}"), "out"));
        actions.push(("Full output".into(), url("/log", &format!("target:job-{id}"), "all", None, None, None), "list"));
    }
    if let Some(u) = r.target.as_deref().and_then(record_url) {
        actions.push(("Open the record".into(), u, "out"));
    }
    match &r.project {
        Some(p) if job.is_some() => actions.push((format!("Filter by project {p}"), filter("project", p), "filter")),
        _ => actions.push((format!("Filter by actor {}", r.actor), filter("actor", &r.actor), "person")),
    }
    if let Some(ip) = &r.ip {
        actions.push((format!("Filter by IP {ip}"), filter("ip", ip), "filter"));
    }
    if r.severity != "info" {
        actions.push((format!("Filter by {}", r.severity), filter("severity", &r.severity), "filter"));
    }
    actions.push((
        "Entries around this".into(),
        url("/log", "", "", Some(r.at - 5 * 60_000), Some(r.at + 5 * 60_000), None),
        "list",
    ));
    if let Some(d) = &r.detail
        && job.is_none()
    {
        fields.push(("Detail".into(), crate::activity::clip(d, 2000)));
    }
    let (job_outcome, job_ok, lines, more_lines) = match &job {
        Some((_, all)) => {
            // "job #3 on hq: cargo test · ok · 1 m 12 s · 120 lines, 1 warnings, 0 errors"
            let outcome = r.summary.split(" · ").skip(1).collect::<Vec<_>>().join(" · ");
            let ok = r.summary.contains(" · ok · ");
            let shown: Vec<(&'static str, String)> =
                all.iter().take(JOB_LINES).map(|l| (super::remote::line_class(l), l.clone())).collect();
            (Some(outcome), ok, shown, all.len().saturating_sub(JOB_LINES))
        }
        None => (None, true, Vec::new(), 0),
    };
    let summary = match &job {
        Some(_) => r.summary.split(" · ").next().unwrap_or(&r.summary).to_owned(),
        None => r.summary.clone(),
    };
    Row {
        seq: r.seq,
        time: tz.hms(r.at),
        cat,
        cat_label,
        cat_icon,
        actor: r.actor.clone(),
        project: r.project.clone().unwrap_or_default(),
        summary,
        severity: r.severity.clone(),
        count: 1,
        archived,
        query: format!("cat:{cat} actor:{}", r.actor),
        fields,
        actions,
        job_outcome,
        job_ok,
        lines,
        more_lines,
    }
}

/// Folds runs of identical entries (same actor, text and second) into one row "×N" (the owner,
/// 2026-11-17: e.g. many `POST /api/sync/state` at once).
fn fold(rows: Vec<(LogRow, bool)>, range: &str, tz: &Tz) -> Vec<Row> {
    let mut out: Vec<(Row, (String, String, i64))> = Vec::new();
    for (r, archived) in rows {
        let key = (r.actor.clone(), r.summary.clone(), r.at / 1000);
        if let Some((last, k)) = out.last_mut()
            && *k == key
            && r.detail.is_none()
        {
            last.count += 1;
            continue;
        }
        out.push((row(&r, archived, range, tz), key));
    }
    out.into_iter().map(|(r, _)| r).collect()
}

/// Rows for a query: from the database, then the archive when the range reaches it.
async fn load(state: &AppState, filter: &Filter, before: Option<i64>) -> anyhow::Result<Vec<(LogRow, bool)>> {
    let f = filter.clone();
    let rows = state.db.call(move |c| logview::page(c, &f, before, PAGE)).await?;
    let mut out: Vec<(LogRow, bool)> = rows.into_iter().map(|r| (r, false)).collect();
    let cutoff = now_ms() - logview::READS_KEPT_DAYS * 86_400_000;
    if filter.from.is_none_or(|f| f < cutoff) && (out.len() as i64) < PAGE {
        let data = state.config.data_dir();
        let (f, b, need) = (filter.clone(), out.last().map(|r| r.0.seq).or(before), (PAGE as usize) - out.len());
        let archived = tokio::task::spawn_blocking(move || logview::search_archive(&data, &f, b, need))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        out.extend(archived.into_iter().map(|r| (r, true)));
    }
    Ok(out)
}

fn time_filter(lq: &LogQuery, range: &str, now: i64) -> Filter {
    let mut filter = parse_query(&lq.q);
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
    filter
}

/// After the last row: where the next ones load from, or the end of the range.
fn after_rows(lq: &LogQuery, range: &str, rows: &[(LogRow, bool)]) -> (Option<String>, End) {
    if rows.len() as i64 >= PAGE {
        let before = rows.last().map(|r| r.0.seq);
        return (Some(url("/log/rows", &lq.q, range, lq.from, lq.to, before)), None);
    }
    let span = match (lq.from, lq.to) {
        (Some(_), Some(_)) => "this stretch".to_owned(),
        _ => RANGES.iter().find(|x| x.0 == range).map(|x| x.1.to_string()).unwrap_or_default(),
    };
    // The next longer range, offered at the end.
    let longer = RANGES
        .iter()
        .skip_while(|x| x.0 != range)
        .nth(1)
        .filter(|_| lq.from.is_none())
        .map(|x| (format!("Show {}", x.1), url("/log", &lq.q, x.0, None, None, None)));
    (None, Some((if range == "all" { "everything".into() } else { span }, longer)))
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
    let filter = time_filter(&lq, &range, now);
    let f = filter.clone();
    let (cats, drops_raw, bars_raw, latest) = state
        .db
        .call(move |c| {
            let cats = logview::category_counts(c, &f)?;
            let mut drops = Vec::new();
            for col in ["project", "actor", "severity"] {
                // Each dropdown counts without its own field, so its other values stay in reach.
                let mut g = f.clone();
                g.fields.retain(|(x, _, _)| x != col);
                drops.push((col, logview::facet(c, &g, col)?));
            }
            let first: i64 = c.query_row("SELECT coalesce(min(at), ?1) FROM log", [now], |r| r.get(0))?;
            let from = f.from.unwrap_or(first);
            let to = f.to.unwrap_or(now + 1);
            let bars = (from, to, logview::histogram(c, &f, from, to, BUCKETS)?);
            Ok((cats, drops, bars, logview::latest_seq(c)?))
        })
        .await?;
    let loaded = load(&state, &filter, lq.before).await?;
    let (next, end) = after_rows(&lq, &range, &loaded);
    let rows = fold(loaded, &range, &tz);

    let picked_cat = field_of(&lq.q, "cat").unwrap_or_default();
    let all_count: i64 = cats.iter().map(|(_, n)| n).sum();
    let mut tabs = vec![Tab {
        label: "All",
        icon: "list",
        count: all_count,
        href: url("/log", &with_field(&lq.q, "cat", ""), &range, lq.from, lq.to, None),
        on: picked_cat.is_empty(),
    }];
    for (key, label) in CATEGORIES {
        let (_, _, icon) = cat_meta(key);
        tabs.push(Tab {
            label,
            icon,
            count: cats.iter().find(|(k, _)| k == key).map(|(_, n)| *n).unwrap_or(0),
            href: url("/log", &with_field(&lq.q, "cat", key), &range, lq.from, lq.to, None),
            on: picked_cat == *key,
        });
    }
    let drops = drops_raw
        .into_iter()
        .map(|(col, values)| {
            let picked = field_of(&lq.q, col).unwrap_or_default();
            Drop {
                label: match col {
                    "project" => "Project",
                    "actor" => "Actor",
                    _ => "Severity",
                },
                all_href: url("/log", &with_field(&lq.q, col, ""), &range, lq.from, lq.to, None),
                values: values
                    .into_iter()
                    .filter(|(v, _)| !v.is_empty())
                    .map(|(v, n)| {
                        let on = picked.eq_ignore_ascii_case(&v);
                        let href = url("/log", &with_field(&lq.q, col, &v), &range, lq.from, lq.to, None);
                        (v, n, href, on)
                    })
                    .collect(),
                picked,
            }
        })
        .collect();

    let (from, to, counts) = bars_raw;
    let max = counts.iter().copied().max().unwrap_or(0).max(1);
    let width = ((to - from) / BUCKETS).max(1);
    let bars = counts
        .iter()
        .enumerate()
        .map(|(i, &n)| {
            let b_from = from + i as i64 * width;
            Bar {
                height: if n == 0 { 0 } else { (n * 100 / max).max(6) },
                href: url("/log", &lq.q, &range, Some(b_from), Some(b_from + width), None),
                label: format!("{n} between {} and {}", tz.short(b_from), tz.short(b_from + width)),
            }
        })
        .collect();
    let axis = (0..=4).map(|i| tz.short(from + (to - from) * i / 4)).collect();
    let total = counts.iter().sum();
    render(LogPage {
        nav: nav(&state, &actor, "/log").await,
        tz: tz.name().to_owned(),
        span: match (lq.from, lq.to) {
            (Some(f), Some(t)) => format!("{} – {}", tz.short(f), tz.short(t)),
            _ if range == "all" => "all time".to_owned(),
            _ => RANGES.iter().find(|x| x.0 == range).map(|x| x.1.to_owned()).unwrap_or_default(),
        },
        ranges: RANGES
            .iter()
            .map(|(k, l, _)| (*k, *l, url("/log", &lq.q, k, None, None, None), *k == range && lq.from.is_none()))
            .collect(),
        // Live while looking at the newest; a past stretch or an older page stays still.
        live: lq.before.is_none() && lq.to.is_none(),
        reset: (!lq.q.is_empty()).then(|| url("/log", "", &range, None, None, None)),
        q: lq.q,
        range,
        tabs,
        drops,
        bars,
        axis,
        total,
        rows,
        next,
        end,
        latest,
        archive: logview::archive_months(&state.config.data_dir()),
    })
}

/// The next rows, for the infinite scroll.
async fn rows(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(lq): Query<LogQuery>,
    tz: Tz,
) -> AppResult<Response> {
    actor.require_owner()?;
    let range = lq.range.clone().filter(|r| RANGES.iter().any(|x| x.0 == r)).unwrap_or_else(|| "24h".into());
    let filter = time_filter(&lq, &range, now_ms());
    let loaded = load(&state, &filter, lq.before).await?;
    let (next, end) = after_rows(&lq, &range, &loaded);
    let rows = fold(loaded, &range, &tz);
    Ok(Html(RowsPart { rows, next, end }.render()?).into_response())
}

#[derive(Deserialize)]
struct StreamQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    range: String,
    after: i64,
}

/// New entries as they're written (server-sent events), each as its rendered row.
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
    let filter = parse_query(&sq.q);
    let range = if sq.range.is_empty() { "24h".to_owned() } else { sq.range };
    let mut after = sq.after;
    tokio::spawn(async move {
        loop {
            let f = filter.clone();
            let rows = state.db.call(move |c| logview::newer(c, &f, after)).await.unwrap_or_default();
            if let Some(last) = rows.last() {
                after = after.max(last.seq);
                // Oldest first from the database; the page puts the newest on top.
                let mut list: Vec<(LogRow, bool)> = rows.into_iter().rev().map(|r| (r, false)).collect();
                list.truncate(200);
                let html =
                    RowsPart { rows: fold(list, &range, &tz), next: None, end: None }.render().unwrap_or_default();
                let data = serde_json::json!({"after": after, "html": html});
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
    fn fields_replace_their_own_and_go() {
        assert_eq!(with_field("severity:info merge", "severity", "warn"), "merge severity:warn");
        assert_eq!(with_field("-action:read", "action", "write"), "action:write");
        assert_eq!(with_field("cat:api merge", "cat", ""), "merge");
        assert_eq!(field_of("merge cat:builds", "cat").as_deref(), Some("builds"));
        assert_eq!(record_url("vinculum/Q-12").as_deref(), Some("/p/vinculum/Q-12"));
        assert_eq!(record_url("../x/Q-1"), None);
        assert_eq!(enc("a b&c"), "a+b%26c");
    }

    #[test]
    fn identical_entries_in_one_second_fold() {
        let tz = Tz::named("UTC").unwrap();
        let r = |seq: i64, at: i64, s: &str| LogRow {
            seq,
            at,
            actor_kind: "project".into(),
            actor: "agentflow".into(),
            project: Some("agentflow".into()),
            via: "api".into(),
            action: "write".into(),
            summary: s.into(),
            target: None,
            status: Some(200),
            duration_ms: None,
            ip: None,
            detail: None,
            severity: "info".into(),
        };
        let rows = vec![
            (r(5, 10_100, "POST /api/sync/state"), false),
            (r(4, 10_050, "POST /api/sync/state"), false),
            (r(3, 10_000, "POST /api/sync/state"), false),
            (r(2, 9_000, "POST /api/sync/state"), false),
            (r(1, 9_000, "POST /api/sync"), false),
        ];
        let folded = fold(rows, "24h", &tz);
        assert_eq!(folded.iter().map(|r| r.count).collect::<Vec<_>>(), vec![3, 1, 1]);
        assert_eq!(folded[0].cat, "api");
    }
}
