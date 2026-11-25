//! The UI library's catalogue at /ui: every token and component in its states, drawn with
//! static/ui.css itself (M15, the owner, 2026-11-23: "build a library of UI components that can be
//! reused"). A component looks here exactly as it does on every page that uses it.

use askama::Template;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Router};

use super::pages::{Nav, nav, render};
use super::{AppResult, AppState};
use crate::auth::Actor;

/// One answer to a question, for the answer control (templates/ui.html, `choices`): the same on
/// the Dashboard, the overview and the question's page (N-19 #1: it was three controls).
pub struct Choice {
    pub n: usize,
    pub title: String,
    pub detail: String,
    pub recommended: bool,
}

/// A long table's pages, for the pager (templates/ui.html, `pager`): "1–20 of 214", page 1 of 11,
/// and the addresses of the pages before and after it.
pub struct Pager {
    /// What it pages, for screen readers: "Pages of the build history".
    pub label: String,
    /// "1–20 of 214"
    pub range: String,
    pub page: i64,
    pub pages: i64,
    pub prev: Option<String>,
    pub next: Option<String>,
    /// What the links lead to: "Newer jobs", "Older jobs".
    pub prev_label: String,
    pub next_label: String,
}

impl Pager {
    /// Page `page` (from 1, kept within the pages there are) of `total` rows, `per` a page; `href`
    /// is a page's address.
    pub fn new(label: &str, total: i64, page: i64, per: i64, href: impl Fn(i64) -> String) -> Self {
        let per = per.max(1);
        let pages = ((total + per - 1) / per).max(1);
        let page = page.clamp(1, pages);
        let first = (page - 1) * per + 1;
        let last = (page * per).min(total);
        Pager {
            label: label.to_owned(),
            range: if total == 0 { "none".to_owned() } else { format!("{first}–{last} of {total}") },
            page,
            pages,
            prev: (page > 1).then(|| href(page - 1)),
            next: (page < pages).then(|| href(page + 1)),
            prev_label: "Previous page".to_owned(),
            next_label: "Next page".to_owned(),
        }
    }

    /// Names the pages before and after ("Newer jobs", "Older jobs").
    pub fn labels(mut self, prev: &str, next: &str) -> Self {
        (self.prev_label, self.next_label) = (prev.to_owned(), next.to_owned());
        self
    }

    /// Where the rows of this page start, for the query's OFFSET.
    pub fn offset(&self, per: i64) -> i64 {
        (self.page - 1) * per.max(1)
    }
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/ui", get(page))
}

type Swatches = &'static [(&'static str, &'static str)];

/// The colour tokens by group: (token, what it's for).
const COLOURS: &[(&str, Swatches)] = &[
    (
        "Surfaces",
        &[
            ("--ground", "the page"),
            ("--sunken", "fields, code, the bar"),
            ("--band", "toolbars, dialog feet"),
            ("--panel", "cards, dialogs"),
            ("--hover-bg", "rows and menus under the pointer"),
            ("--chip", "chips, neutral notices"),
            ("--selected", "the picked row or nav item"),
        ],
    ),
    (
        "Lines",
        &[
            ("--line2", "between rows"),
            ("--line", "cards"),
            ("--border", "controls"),
            ("--accent-line", "set and picked controls"),
        ],
    ),
    ("Ink", &[("--ink", "text"), ("--ink2", "secondary text"), ("--muted", "hints, labels"), ("--dim", "disabled")]),
    (
        "Accent",
        &[
            ("--accent", "primary, links"),
            ("--accent-ink", "hover, accent text"),
            ("--accent-bg", "accent surfaces"),
            ("--accent-chip", "picked switches, chips"),
            ("--accent-deep", "past bars"),
        ],
    ),
    (
        "Status",
        &[
            ("--ok", "works, done"),
            ("--warn", "waiting on you"),
            ("--bad", "failed, destructive"),
            ("--info", "drafts, information"),
        ],
    ),
    (
        "Charts",
        &[
            ("--cat-1", "a project"),
            ("--cat-2", "a project"),
            ("--cat-3", "a project"),
            ("--cat-4", "a project"),
            ("--cat-5", "a project"),
            ("--cat-other", "the rest"),
        ],
    ),
];

/// The type roles: (token, size / weight, the class that sets it, sample, where).
const TYPE: &[(&str, &str, &str, &str, &str)] = &[
    ("--fs-display", "30 / 600", "t-display", "Usage", "Page titles; big numbers in mono 500"),
    ("--fs-h1", "24 / 600", "t-h1", "The Log's top is one card", "Record titles, phone page titles"),
    ("--fs-h2", "18 / 600", "t-h2", "Vinculum: settings", "Page sections, dialog titles"),
    ("--fs-h3", "16 / 600", "t-h3", "Per project", "Card titles"),
    ("--fs-body", "15 / 400", "", "An answered question becomes a decision.", "Body text, fields, buttons"),
    ("--fs-sm", "14 / 400", "t-sm", "Rows, tables, notices, menus", "Dense UI"),
    ("--fs-sub", "13 / 400", "sub", "newest call 12 min ago", "Secondary lines, code"),
    ("--fs-label", "12 / 600 caps", "label", "Today · Mon 23 Nov", "Group heads, table heads"),
    ("--fs-micro", "11 / 500", "t-micro", "M14", "Tags, badges"),
];

/// State words, each drawn in the tone src/web/tone.rs gives it.
const STATES: &[&str] = &[
    "todo", "doing", "review", "blocked", "done", "parked", "skipped", "draft", "running", "paused", "queued",
    "failed", "open", "answered", "proposed", "approved", "built", "current", "replaced",
];

/// The button variants: (class, label, where).
const BUTTONS: &[(&str, &str, &str)] = &[
    ("", "Merge", "One per view: Save, Merge, Push tag"),
    ("secondary", "Rename", "Other actions beside a primary"),
    ("quiet", "Cancel", "Changes nothing: Cancel, Close"),
    ("danger", "Remove", "Every destructive action"),
    ("danger-solid", "Delete", "Only the confirm of a confirm dialog"),
];

#[derive(Template)]
#[template(path = "uilib.html")]
struct UiPage {
    nav: Nav,
    colours: &'static [(&'static str, Swatches)],
    type_roles: &'static [(&'static str, &'static str, &'static str, &'static str, &'static str)],
    buttons: &'static [(&'static str, &'static str, &'static str)],
    states: &'static [&'static str],
    choices: Vec<Choice>,
    spacing: &'static [u32],
    radii: &'static [(&'static str, &'static str)],
    icons: &'static [&'static str],
    pager: Pager,
}

impl UiPage {
    /// An icon at one of the four sizes ("xs", "sm", "" or "lg").
    fn icon_sized(&self, name: &str, size: &str) -> String {
        let svg = super::icons::icon(name);
        if size.is_empty() { svg.to_owned() } else { svg.replacen(r#"class="i""#, &format!(r#"class="i {size}""#), 1) }
    }
}

async fn page(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> AppResult<Response> {
    actor.require_owner()?;
    render(UiPage {
        nav: nav(&state, &actor, "/ui").await,
        colours: COLOURS,
        type_roles: TYPE,
        buttons: BUTTONS,
        states: STATES,
        choices: vec![
            Choice {
                n: 1,
                title: "One Decisions tab: questions on top, answered in place".into(),
                detail: "The answer becomes a decision quoting you".into(),
                recommended: true,
            },
            Choice { n: 2, title: "Both stay, each made better".into(), detail: String::new(), recommended: false },
        ],
        spacing: &[2, 4, 6, 8, 12, 16, 20, 24, 32, 48],
        radii: &[("xs", "4"), ("sm", "6"), ("md", "8"), ("lg", "12"), ("xl", "16"), ("pill", "999")],
        icons: super::icons::ALL,
        pager: Pager::new("Pages of the example", 214, 1, 20, |p| format!("/ui?page={p}#ui-table"))
            .labels("Newer jobs", "Older jobs"),
    })
}

#[cfg(test)]
mod tests {
    use super::Pager;

    #[test]
    fn a_pager_counts_its_rows_and_pages() {
        let p = Pager::new("x", 214, 1, 20, |p| format!("?page={p}"));
        assert_eq!((p.range.as_str(), p.page, p.pages), ("1–20 of 214", 1, 11));
        assert_eq!((p.prev, p.next.as_deref()), (None, Some("?page=2")));
        let p = Pager::new("x", 214, 99, 20, |p| format!("?page={p}"));
        assert_eq!((p.range.as_str(), p.page, p.offset(20)), ("201–214 of 214", 11, 200));
        assert_eq!((p.prev.as_deref(), p.next), (Some("?page=10"), None));
        let p = Pager::new("x", 0, 0, 20, |p| format!("?page={p}"));
        assert_eq!((p.range.as_str(), p.page, p.pages, p.offset(20)), ("none", 1, 1, 0));
    }
}
