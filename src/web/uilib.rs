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
    spacing: &'static [u32],
    radii: &'static [(&'static str, &'static str)],
    icons: &'static [&'static str],
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
        spacing: &[2, 4, 6, 8, 12, 16, 20, 24, 32, 48],
        radii: &[("xs", "4"), ("sm", "6"), ("md", "8"), ("lg", "12"), ("xl", "16"), ("pill", "999")],
        icons: super::icons::ALL,
    })
}
