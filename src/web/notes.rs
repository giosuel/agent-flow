//! Notes over HTTP: agents upload files (`flow note`), pages offer them for download.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{AppState, json_error};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::notes;
use crate::records::{self, Kind, NewRecord, Patch, RecordRef};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/notes", post(upload).layer(DefaultBodyLimit::max(notes::MAX_BYTES + 1)))
        .route("/p/{slug}/{id}/file", get(file))
}

#[derive(Deserialize)]
struct UploadQuery {
    name: String,
    category: String,
    /// The file's own name, for its extension and the download.
    filename: String,
    #[serde(default)]
    description: String,
    /// A record of the project it belongs to, e.g. T-12 or a step's name isn't a record: use the
    /// description for those.
    #[serde(default)]
    record: String,
    #[serde(default)]
    resurface: String,
    /// A new version of this note's file, e.g. N-6.
    #[serde(default)]
    replaces: String,
}

async fn upload(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    let name = q.name.split_whitespace().collect::<Vec<_>>().join(" ");
    if q.replaces.trim().is_empty() && (name.is_empty() || name.chars().count() > 120) {
        return json_error(StatusCode::BAD_REQUEST, "give it a name (up to 120 characters)");
    }
    if body.len() > notes::MAX_BYTES {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "a note's file can be 25 MB at most");
    }
    if body.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "the file is empty");
    }
    let category = if q.replaces.trim().is_empty() || !q.category.trim().is_empty() {
        match crate::screenshots::category(&q.category) {
            Ok(c) => Some(c),
            Err(_) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "give it a category, e.g. morning report, estimate, known issue",
                );
            }
        }
    } else {
        None
    };
    let resurface = match q.resurface.trim() {
        "" => None,
        r => match notes::resurface(r) {
            Ok(x) => Some(x),
            Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
        },
    };
    let data_dir = state.config.data_dir();
    let free = rustix::fs::statvfs(&data_dir).map(|s| s.f_bavail.saturating_mul(s.f_frsize)).unwrap_or(0);
    if free < (state.config.build_free_gb << 30) + body.len() as u64 {
        return json_error(StatusCode::INSUFFICIENT_STORAGE, "the server's disk is nearly full; not storing it");
    }
    let ext = notes::ext_of(&q.filename);
    let incoming = notes::dir(&data_dir).join(".incoming");
    let tmp = incoming.join(format!("{}.{ext}", crate::util::new_secret("")));
    let written = async {
        tokio::fs::create_dir_all(&incoming).await?;
        tokio::fs::write(&tmp, &body).await
    }
    .await;
    if let Err(e) = written {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"));
    }
    let (a, size, tmp2, dd) = (actor.clone(), body.len(), tmp.clone(), data_dir.clone());
    let filename: String = q.filename.chars().filter(|c| !c.is_control()).take(120).collect();
    let created = state
        .db
        .call(move |c| {
            let project = crate::mcp::target_project(c, &a, None, true)?;
            let mut data = Map::new();
            if let Some(cat) = &category {
                data.insert("category".into(), cat.clone().into());
            }
            data.insert("ext".into(), ext.clone().into());
            data.insert("size".into(), size.into());
            data.insert("filename".into(), filename.clone().into());
            if let Some((at, text)) = &resurface {
                data.insert("resurface".into(), text.clone().into());
                data.insert("resurface_at".into(), at.map(Value::from).unwrap_or(Value::Null));
            }
            // A new version: the old file is kept beside it, numbered.
            if !q.replaces.trim().is_empty() {
                let old = records::find_str(c, q.replaces.trim(), Some(&project.slug))?;
                if old.kind != Kind::Note || old.project_id != project.id {
                    anyhow::bail!("{} isn't a note of {}", old.short_id(), project.slug);
                }
                if old.str("ext").is_empty() {
                    anyhow::bail!("{} is a text note; change it with update_note", old.short_id());
                }
                let version = old.data.get("versions").and_then(Value::as_i64).unwrap_or(0) + 1;
                let (from, to) = (
                    notes::path(&dd, &project.slug, old.number, old.str("ext"), 0),
                    notes::path(&dd, &project.slug, old.number, old.str("ext"), version),
                );
                std::fs::rename(&from, &to)?;
                data.insert("versions".into(), version.into());
                let new_path = notes::path(&dd, &project.slug, old.number, &ext, 0);
                std::fs::rename(&tmp2, &new_path)?;
                let title = if name.is_empty() { None } else { Some(name.clone()) };
                let (r, _) = records::update(
                    c,
                    &old,
                    &a.name(),
                    Patch { title, data, note: format!("new version of the file ({filename})"), ..Default::default() },
                )?;
                return Ok(r);
            }
            // A record (T-12) or a step (P9.R): a note can belong to either.
            let (shows, step) = crate::notes::target(c, &project, q.record.trim())?;
            let r = records::create(
                c,
                project.id,
                &a.name(),
                NewRecord {
                    kind: Some(Kind::Note),
                    title: name,
                    body: q.description.trim().to_owned(),
                    milestone_id: step,
                    data,
                    ..Default::default()
                },
            )?;
            let to = notes::path(&dd, &project.slug, r.number, &ext, 0);
            std::fs::create_dir_all(to.parent().unwrap_or(&dd))?;
            if let Err(e) = std::fs::rename(&tmp2, &to) {
                let patch = Patch {
                    state: Some("resolved".into()),
                    note: "the file couldn't be stored".into(),
                    ..Default::default()
                };
                records::update(c, &r, &a.name(), patch)?;
                return Err(e.into());
            }
            if let Some(target) = &shows {
                records::link(c, &r, target, "relates", &a.name())?;
            }
            Ok(r)
        })
        .await;
    let r = match created {
        Ok(r) => r,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return json_error(StatusCode::BAD_REQUEST, format!("{e:#}"));
        }
    };
    let e = Entry::new(&actor, Via::Api, Action::Write, format!("note {}: {}", r.short_id(), r.title))
        .project(r.project.clone())
        .target(r.ref_id.clone());
    activity::write(&state.db, e).await;
    let url = format!("{}{}", state.config.public_url.trim_end_matches('/'), r.url());
    Json(json!({"id": r.ref_id, "number": r.number, "url": url})).into_response()
}

#[derive(Deserialize)]
struct FileQuery {
    /// An older version.
    v: Option<i64>,
}

/// A note's file, always as a download (text types as plain text), never as a page.
async fn file(
    State(state): State<AppState>,
    Path((slug, id)): Path<(String, String)>,
    Query(q): Query<FileQuery>,
) -> Response {
    let s = slug.clone();
    let found = state
        .db
        .call(move |c| {
            let Ok(rref) = RecordRef::parse(&format!("{s}/{id}")) else { return Ok(None) };
            Ok(records::try_find(c, &rref, None)?.filter(|r| r.kind == Kind::Note))
        })
        .await;
    let Ok(Some(r)) = found else { return StatusCode::NOT_FOUND.into_response() };
    let ext = r.str("ext").to_owned();
    if ext.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let v = q.v.unwrap_or(0).max(0);
    let path = notes::path(&state.config.data_dir(), &slug, r.number, &ext, v);
    let Ok(bytes) = tokio::fs::read(&path).await else { return StatusCode::NOT_FOUND.into_response() };
    let mime = if notes::readable(&ext) { "text/plain; charset=utf-8" } else { "application/octet-stream" };
    let name: String = r.str("filename").chars().filter(|c| c.is_ascii_alphanumeric() || "-_. ".contains(*c)).collect();
    let name = if name.trim().is_empty() { format!("N-{}.{ext}", r.number) } else { name };
    let mut res = bytes.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox; default-src 'none'"));
    res
}

/// A note's text for its page: a text note's body, or a readable file (up to 1 MB).
pub fn text_of(data_dir: &std::path::Path, r: &records::Record) -> Option<(String, bool)> {
    let ext = r.str("ext");
    if ext.is_empty() {
        return Some((r.body.clone(), true));
    }
    if !notes::readable(ext) || r.data.get("size").and_then(Value::as_i64).unwrap_or(0) > 1024 * 1024 {
        return None;
    }
    let bytes = std::fs::read(notes::path(data_dir, &r.project, r.number, ext, 0)).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    Some((text, matches!(ext, "md" | "markdown")))
}

/// The Notes tab's search and filters (the owner, 2026-11-18: "an option to better filter and
/// search for files, similar to how logs look"), from the address: `nq` (words, `category:…`,
/// `is:file|note|resolved|current`, `ext:…`), `ncat` (categories, `|` between them), `nkind`
/// (file, note), `nstate` (current by default, resolved, all) and `nrange` (24h, 7d, 30d; all by
/// default).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    pub q: String,
    pub cats: Vec<String>,
    pub kind: String,
    pub state: String,
    pub range: String,
}

impl Filter {
    pub fn new(
        q: Option<&str>,
        cats: Option<&str>,
        kind: Option<&str>,
        state: Option<&str>,
        range: Option<&str>,
    ) -> Self {
        let pick = |v: Option<&str>, ok: &[&str]| v.filter(|v| ok.contains(v)).unwrap_or("").to_owned();
        Filter {
            q: q.unwrap_or("").trim().chars().take(200).collect(),
            cats: cats.unwrap_or("").split('|').map(str::trim).filter(|c| !c.is_empty()).map(str::to_owned).collect(),
            kind: pick(kind, &["file", "note"]),
            state: pick(state, &["resolved", "all"]),
            range: pick(range, &["24h", "7d", "30d"]),
        }
    }

    /// The tab's address with these filters.
    pub fn href(&self, slug: &str) -> String {
        use super::log::enc;
        let mut u = format!("/p/{slug}?tab=notes");
        if !self.q.is_empty() {
            u += &format!("&nq={}", enc(&self.q));
        }
        if !self.cats.is_empty() {
            u += &format!("&ncat={}", enc(&self.cats.join("|")));
        }
        for (k, v) in [("nkind", &self.kind), ("nstate", &self.state), ("nrange", &self.range)] {
            if !v.is_empty() {
                u += &format!("&{k}={v}");
            }
        }
        u
    }

    fn with(&self, f: impl FnOnce(&mut Filter)) -> Filter {
        let mut x = self.clone();
        f(&mut x);
        x
    }

    fn range_ms(&self) -> Option<i64> {
        match self.range.as_str() {
            "24h" => Some(86_400_000),
            "7d" => Some(7 * 86_400_000),
            "30d" => Some(30 * 86_400_000),
            _ => None,
        }
    }
}

/// What a file's type looks like: an icon and a tint (the owner, 2026-11-18: "since the
/// categories can be freely chosen, lets make the icon dependent on the file type").
pub fn type_icon(ext: &str) -> (&'static str, &'static str) {
    match ext.to_ascii_lowercase().as_str() {
        "" => ("note", "teal"),
        "md" | "markdown" => ("ftext", "violet"),
        "txt" | "log" | "rst" | "adoc" => ("ftext", "gray"),
        "gz" | "tgz" | "zip" | "tar" | "xz" | "zst" | "bz2" | "7z" => ("archive", "orange"),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "avif" => ("image", "blue"),
        "csv" | "tsv" | "xlsx" | "ods" | "json" | "toml" | "yaml" | "yml" => ("table", "green"),
        "pdf" => ("pdf", "red"),
        "rs" | "py" | "js" | "ts" | "sh" | "html" | "css" | "patch" | "diff" => ("code", "blue"),
        _ => ("file", "gray"),
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// `text`, escaped, with every search word marked.
fn marked(text: &str, words: &[String]) -> String {
    let lower = text.to_lowercase();
    // Lowercasing can change byte lengths (rare letters); then nothing is marked.
    if lower.len() != text.len() || words.is_empty() {
        return esc(text);
    }
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for w in words.iter().filter(|w| !w.is_empty()) {
        let mut from = 0;
        while let Some(i) = lower[from..].find(w.as_str()) {
            let a = from + i;
            spans.push((a, a + w.len()));
            from = a + w.len();
        }
    }
    spans.sort();
    let mut out = String::new();
    let mut at = 0;
    for (a, b) in spans {
        if a < at || !text.is_char_boundary(a) || !text.is_char_boundary(b) {
            continue;
        }
        out += &esc(&text[at..a]);
        out += &format!("<mark>{}</mark>", esc(&text[a..b]));
        at = b;
    }
    out += &esc(&text[at..]);
    out
}

/// A line of a note's text: Markdown marks left out, on one line; around the first search word
/// when it's further in.
fn snippet(body: &str, words: &[String]) -> String {
    let flat: String = body
        .lines()
        .map(|l| l.trim().trim_start_matches(['#', '>', '-', '*', ' ']).trim())
        .filter(|l| !l.is_empty() && !l.starts_with("```"))
        .collect::<Vec<_>>()
        .join(" ")
        .replace(['`', '*'], "");
    let lower = flat.to_lowercase();
    let hit = words.iter().filter_map(|w| lower.find(w.as_str())).min().filter(|_| lower.len() == flat.len());
    let start = match hit {
        Some(i) if i > 90 => {
            let mut s = i - 50;
            while !flat.is_char_boundary(s) {
                s -= 1;
            }
            s
        }
        _ => 0,
    };
    let rest: String = flat[start..].chars().take(220).collect();
    if start > 0 { format!("…{rest}") } else { rest }
}

/// A note in the Notes tab's table.
pub struct NoteRow {
    pub short: String,
    pub url: String,
    /// The title and the line under it, escaped, with the search words marked.
    pub title_html: String,
    pub snippet_html: String,
    pub category: String,
    pub icon: &'static str,
    pub tint: &'static str,
    /// "md · 5 KB", or "note".
    pub kind: String,
    pub resurface: String,
    pub added: String,
    pub resolved: bool,
    pub download: String,
}

/// One choice of a filter's menu: (label, count or "", href, picked).
pub type Choice = (String, String, String, bool);

pub struct NotesView {
    pub q: String,
    /// Hidden fields that keep the other filters when searching.
    pub keep: Vec<(&'static str, String)>,
    pub cat_label: String,
    pub cats: Vec<Choice>,
    pub cats_clear: String,
    pub kind_label: String,
    pub kinds: Vec<Choice>,
    pub state_label: String,
    pub states: Vec<Choice>,
    pub ranges: Vec<Choice>,
    pub reset: Option<String>,
    pub rows: Vec<NoteRow>,
    /// "37 · 6 files to download", or "3 found".
    pub summary: String,
    /// Under the table: what's hidden.
    pub foot: String,
    /// The project has no notes at all.
    pub none: bool,
}

/// Does a note match the search words and `key:value` terms?
fn matches(r: &records::Record, terms: &[(String, String)]) -> bool {
    let hay =
        format!("{} {} {} {} {}", r.short_id(), r.title, r.body, r.str("category"), r.str("filename")).to_lowercase();
    terms.iter().all(|(k, v)| match k.as_str() {
        "category" | "cat" => r.str("category").to_lowercase().contains(v.as_str()),
        "ext" | "type" => r.str("ext").eq_ignore_ascii_case(v),
        "is" => match v.as_str() {
            "file" => !r.str("ext").is_empty(),
            "note" => r.str("ext").is_empty(),
            "resolved" => r.state != "current",
            "current" | "open" => r.state == "current",
            _ => true,
        },
        _ => hay.contains(v.as_str()),
    })
}

fn terms_of(q: &str) -> Vec<(String, String)> {
    q.split_whitespace()
        .map(|t| {
            let t = t.to_lowercase();
            match t.split_once(':') {
                Some((k, v)) if ["category", "cat", "ext", "type", "is"].contains(&k) && !v.is_empty() => {
                    (k.to_owned(), v.trim_start_matches('.').to_owned())
                }
                _ => (String::new(), t),
            }
        })
        .collect()
}

pub fn view(c: &rusqlite::Connection, slug: &str, project_id: i64, f: &Filter, now: i64) -> anyhow::Result<NotesView> {
    let all = records::query(
        c,
        &records::Query { project_id: Some(project_id), kinds: vec![Kind::Note], limit: 0, ..Default::default() },
    )?;
    let terms = terms_of(&f.q);
    let words: Vec<String> = terms.iter().filter(|(k, _)| k.is_empty()).map(|(_, v)| v.clone()).collect();
    // The State filter is set by the search too ("is:resolved").
    let state_in_q = terms.iter().any(|(k, v)| k == "is" && ["resolved", "current", "open"].contains(&v.as_str()));
    let since = f.range_ms().map(|ms| now - ms);
    let kind_ok = |r: &records::Record| match f.kind.as_str() {
        "file" => !r.str("ext").is_empty(),
        "note" => r.str("ext").is_empty(),
        _ => true,
    };
    let state_ok = |r: &records::Record| {
        state_in_q
            || match f.state.as_str() {
                "all" => true,
                "resolved" => r.state != "current",
                _ => r.state == "current",
            }
    };
    let cat_ok = |r: &records::Record| f.cats.is_empty() || f.cats.iter().any(|c| c == r.str("category"));
    let time_ok = |r: &records::Record| since.is_none_or(|s| r.created_at >= s);
    let search_ok = |r: &records::Record| matches(r, &terms);

    // Each menu counts what its own choices would show, with the other filters as they are.
    let mut cat_n: Vec<(String, usize)> = Vec::new();
    for r in all.iter().filter(|r| kind_ok(r) && state_ok(r) && time_ok(r) && search_ok(r)) {
        let cat = r.str("category").to_owned();
        match cat_n.iter_mut().find(|x| x.0 == cat) {
            Some(x) => x.1 += 1,
            None => cat_n.push((cat, 1)),
        }
    }
    // A picked category stays in the menu even when nothing else matches it.
    for c in &f.cats {
        if !cat_n.iter().any(|x| &x.0 == c) {
            cat_n.push((c.clone(), 0));
        }
    }
    cat_n.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let cats = cat_n
        .into_iter()
        .map(|(cat, n)| {
            let on = f.cats.contains(&cat);
            let next = f.with(|x| {
                if on {
                    x.cats.retain(|c| c != &cat);
                } else {
                    x.cats.push(cat.clone());
                }
            });
            (cat, n.to_string(), next.href(slug), on)
        })
        .collect();
    let count = |pred: &dyn Fn(&records::Record) -> bool| all.iter().filter(|r| pred(r)).count().to_string();
    let kinds = [("", "Files and notes"), ("file", "Files"), ("note", "Notes")]
        .into_iter()
        .map(|(k, label)| {
            let n = count(&|r| {
                cat_ok(r)
                    && state_ok(r)
                    && time_ok(r)
                    && search_ok(r)
                    && match k {
                        "file" => !r.str("ext").is_empty(),
                        "note" => r.str("ext").is_empty(),
                        _ => true,
                    }
            });
            (label.to_owned(), n, f.with(|x| x.kind = k.into()).href(slug), f.kind == k)
        })
        .collect();
    let states = [("", "Current"), ("resolved", "Resolved"), ("all", "All")]
        .into_iter()
        .map(|(k, label)| {
            let n = count(&|r| {
                cat_ok(r)
                    && kind_ok(r)
                    && time_ok(r)
                    && search_ok(r)
                    && match k {
                        "all" => true,
                        "resolved" => r.state != "current",
                        _ => r.state == "current",
                    }
            });
            (label.to_owned(), n, f.with(|x| x.state = k.into()).href(slug), f.state == k)
        })
        .collect();
    let ranges = [("24h", "24 h"), ("7d", "7 d"), ("30d", "30 d"), ("", "All")]
        .into_iter()
        .map(|(k, label)| (label.to_owned(), String::new(), f.with(|x| x.range = k.into()).href(slug), f.range == k))
        .collect();

    let shown: Vec<&records::Record> =
        all.iter().filter(|r| cat_ok(r) && kind_ok(r) && state_ok(r) && time_ok(r) && search_ok(r)).collect();
    let mut rows = Vec::new();
    for r in &shown {
        let ext = r.str("ext").to_owned();
        let (icon, tint) = type_icon(&ext);
        let size = r.data.get("size").and_then(Value::as_i64).map(notes::size);
        rows.push(NoteRow {
            short: r.short_id(),
            url: r.url(),
            title_html: marked(&r.title, &words),
            snippet_html: marked(&snippet(&r.body, &words), &words),
            category: r.str("category").to_owned(),
            icon,
            tint,
            kind: match (&ext[..], size) {
                ("", _) => "note".into(),
                (e, Some(s)) => format!("{e} · {s}"),
                (e, None) => e.to_owned(),
            },
            resurface: match (r.str("resurface"), r.data.get("resurface_at").and_then(Value::as_i64)) {
                ("", _) => String::new(),
                (_, Some(at)) => {
                    format!("Comes back {}", crate::tz::Tz::named("UTC").map(|t| t.date(at)).unwrap_or_default())
                }
                (w, None) => format!("Comes back {w}"),
            },
            added: crate::util::ago(r.created_at, now),
            resolved: r.state != "current",
            download: if ext.is_empty() { String::new() } else { format!("{}/file", r.url()) },
        });
    }
    let filtered = !f.q.is_empty() || !f.cats.is_empty() || !f.kind.is_empty() || !f.range.is_empty();
    let files = rows.iter().filter(|r| !r.download.is_empty()).count();
    let summary = if filtered {
        format!("{} found", rows.len())
    } else if files > 0 {
        format!("{} · {files} file{} to download", rows.len(), if files == 1 { "" } else { "s" })
    } else {
        rows.len().to_string()
    };
    let hidden_resolved = if f.state.is_empty() && !state_in_q {
        all.iter().filter(|r| r.state != "current" && cat_ok(r) && kind_ok(r) && time_ok(r) && search_ok(r)).count()
    } else {
        0
    };
    let foot = match (rows.is_empty(), hidden_resolved) {
        (_, 0) if all.is_empty() => String::new(),
        (true, 0) => "Nothing matches.".into(),
        (true, n) => format!("Nothing current matches; {n} resolved hidden (State)."),
        (false, 0) => String::new(),
        (false, n) => format!("{n} resolved hidden (State)."),
    };
    let label = |picked: Vec<&str>| if picked.is_empty() { "All".to_owned() } else { picked.join(", ") };
    let mut keep = Vec::new();
    for (k, v) in [
        ("ncat", f.cats.join("|")),
        ("nkind", f.kind.clone()),
        ("nstate", f.state.clone()),
        ("nrange", f.range.clone()),
    ] {
        if !v.is_empty() {
            keep.push((k, v));
        }
    }
    Ok(NotesView {
        q: f.q.clone(),
        keep,
        cat_label: if f.cats.len() > 2 {
            format!("{} picked", f.cats.len())
        } else {
            label(f.cats.iter().map(String::as_str).collect())
        },
        cats,
        cats_clear: f.with(|x| x.cats.clear()).href(slug),
        kind_label: match f.kind.as_str() {
            "file" => "Files".into(),
            "note" => "Notes".into(),
            _ => "Files and notes".into(),
        },
        kinds,
        state_label: match f.state.as_str() {
            "resolved" => "Resolved".into(),
            "all" => "All".into(),
            _ => "Current".into(),
        },
        states,
        ranges,
        reset: (filtered || !f.state.is_empty()).then(|| format!("/p/{slug}?tab=notes")),
        rows,
        summary,
        foot,
        none: all.is_empty(),
    })
}

/// A note on its own page: its text (a text note, or a readable file), and the file to download.
pub struct NotePage {
    pub text_html: String,
    pub plain: String,
    pub download: String,
    pub filename: String,
    pub size: String,
    pub category: String,
    pub resurface: String,
    pub versions: Vec<(i64, String)>,
}

pub fn page(data_dir: &std::path::Path, r: &records::Record) -> NotePage {
    let ext = r.str("ext");
    let (text_html, plain) = match text_of(data_dir, r) {
        Some((t, true)) => (crate::markdown::to_html(&t), String::new()),
        Some((t, false)) => (String::new(), t),
        None => (String::new(), String::new()),
    };
    let versions = r.data.get("versions").and_then(Value::as_i64).unwrap_or(0);
    NotePage {
        text_html,
        plain,
        download: if ext.is_empty() { String::new() } else { format!("{}/file", r.url()) },
        filename: r.str("filename").to_owned(),
        size: r.data.get("size").and_then(Value::as_i64).map(notes::size).unwrap_or_default(),
        category: r.str("category").to_owned(),
        resurface: r.str("resurface").to_owned(),
        versions: (1..=versions).rev().map(|v| (v, format!("{}/file?v={v}", r.url()))).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_round_trip_through_the_address_and_search_words_are_marked_safely() {
        let f = Filter::new(Some(" flaky "), Some("known issue|plan"), Some("file"), Some("bogus"), Some("7d"));
        assert_eq!(f.href("im"), "/p/im?tab=notes&nq=flaky&ncat=known+issue%7Cplan&nkind=file&nrange=7d");
        assert_eq!(f.state, "", "unknown values are left out");
        assert_eq!(marked("Flaky <b> test", &["flaky".into()]), "<mark>Flaky</mark> &lt;b&gt; test");
        assert_eq!(marked("a & b", &["&".into()]), "a <mark>&amp;</mark> b");
        let long = format!("{} the word flaky here", "x ".repeat(80));
        assert!(snippet(&long, &["flaky".into()]).starts_with('…'));
        assert_eq!(snippet("# Title\n\n- **bold** `code`", &[]), "Title bold code");
        assert_eq!(type_icon("MD"), ("ftext", "violet"));
        assert_eq!(type_icon(""), ("note", "teal"));
        assert_eq!(type_icon("tar"), ("archive", "orange"));
    }
}
