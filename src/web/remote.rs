//! Build servers over HTTP: agents start jobs (`flow remote`) and follow their output; the
//! owner watches the Machine page's Build servers section and each job's page, and can pause builds
//! or cancel a job. See `crate::remote`.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::pages::{Nav, nav, render};
use super::{AppError, AppResult, AppState, json_error};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::remote::{self, Job, NewJob};
use crate::util::{ago, now_ms};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/remote/jobs", post(start))
        .route("/api/remote/jobs/{id}/output", get(output_api))
        .route("/api/remote/jobs/{id}/cancel", post(cancel_api))
        .route("/machine/builds", get(section))
        .route("/machines/jobs/{id}", get(job_page))
        .route("/machines/jobs/{id}/output", get(output))
        .route("/machines/jobs/{id}/cancel", post(cancel_page))
        .route("/machines/{name}/pause", post(pause))
        .route("/machines/{name}/resume", post(resume))
        // The addresses from before Machines (D-277): agents' terminals and the Log hold job links,
        // and a page open since then posts its forms there.
        .route("/machine/jobs/{id}", get(|Path(id): Path<i64>| async move { Redirect::to(&job_url(id)) }))
        .route("/machine/jobs/{id}/output", get(output))
        .route("/machine/jobs/{id}/cancel", post(cancel_page))
        .route("/machine/servers/{name}/pause", post(pause))
        .route("/machine/servers/{name}/resume", post(resume))
        .route("/settings/build-servers/{name}/limits", post(save_limits))
        .route("/settings/build-servers/{name}/rename", post(rename_server))
        .route("/settings/build-servers/{name}/remove", post(remove_server))
}

/// A job's page.
pub fn job_url(id: i64) -> String {
    format!("/machines/jobs/{id}")
}

#[derive(Deserialize)]
struct StartJob {
    /// What it is, in a few words ("Tests for PR #170"): its title (D-275). Required.
    #[serde(default)]
    name: String,
    /// The checkout (or a folder in it) the command runs in, on this machine.
    dir: String,
    command: Vec<String>,
    #[serde(default)]
    fetch: Vec<String>,
    #[serde(default)]
    gpu: bool,
    /// Which build server; the first if left out.
    #[serde(default)]
    server: Option<String>,
    /// A snapshot `flow remote` uploaded (its manifest's SHA-256), so the server needn't read
    /// `dir`: the agent may work on any machine.
    #[serde(default)]
    manifest: String,
    /// New commits and tags as a git bundle in the store, or "".
    #[serde(default)]
    bundle: String,
    /// Where in the checkout to run, with a snapshot.
    #[serde(default)]
    subdir: String,
    /// `flow remote --setup`: run in the builder's home, to install tools.
    #[serde(default)]
    setup: bool,
}

/// An agent starts a job. Refused at once when the server is offline, so the agent builds locally.
async fn start(State(state): State<AppState>, Extension(actor): Extension<Actor>, Json(s): Json<StartJob>) -> Response {
    let Actor::Project { project_id, slug, .. } = &actor else {
        return json_error(StatusCode::FORBIDDEN, "only project agents run jobs on build servers");
    };
    let servers = remote::servers(&state);
    // The one asked for, else the one the owner picked for the project in its Settings.
    let pid = *project_id;
    let picked = state.db.call(move |c| remote::project_server(c, pid)).await.ok().flatten();
    let wanted = s.server.clone().or(picked);
    let Some(server) = wanted.as_ref().and_then(|n| servers.iter().find(|b| &b.name == n)) else {
        // Nothing picked: said apart from offline, naming the builders that are online, so an agent
        // doesn't quietly build here while one waits (the owner, 2026-11-17).
        let online: Vec<String> =
            servers.iter().map(|b| b.name.clone()).filter(|n| crate::builders::hub().is_online(n)).collect();
        let why = match (&wanted, online.first()) {
            (Some(n), _) => format!("there's no build server called {n}"),
            (None, Some(first)) => format!(
                "no build server is picked for this project (the owner picks one in its Settings); online now: {} (flow remote --server {first} --name \"…\" -- …)",
                online.join(", ")
            ),
            (None, None) => "no build server is picked for this project and none is online; build here instead".into(),
        };
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": why, "offline": false, "none_picked": true, "online": online})),
        )
            .into_response();
    };
    if !crate::builders::hub().is_online(&server.name) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": format!("{} is offline; build here instead", server.name), "offline": true})),
        )
            .into_response();
    }
    // The builder takes a snapshot: what runs comes with the job, the server reads no checkout.
    let new = {
        if !s.setup && s.manifest.is_empty() {
            return json_error(
                StatusCode::BAD_REQUEST,
                "this builder takes a snapshot of your checkout: update flow and run flow remote again",
            );
        }
        if !s.manifest.is_empty() {
            let store = crate::blobs::Store::new(crate::web::builders::store_dir(&state));
            if !store.has(&s.manifest) || (!s.bundle.is_empty() && !store.has(&s.bundle)) {
                return json_error(StatusCode::BAD_REQUEST, "upload the snapshot first (flow remote does)");
            }
        }
        if !s.subdir.is_empty() && crate::blobs::check_path(&s.subdir).is_err() {
            return json_error(StatusCode::BAD_REQUEST, "subdir is a folder inside the checkout");
        }
        NewJob {
            name: s.name,
            server: server.name.clone(),
            project_id: *project_id,
            actor: slug.clone(),
            dir: s.dir.chars().take(500).collect(),
            subdir: if s.setup { String::new() } else { s.subdir },
            command: s.command,
            fetch: if s.setup { Vec::new() } else { s.fetch },
            gpu: s.gpu,
            manifest: s.manifest,
            bundle: s.bundle,
            setup: s.setup,
        }
    };
    let created = state.db.call(move |c| remote::create(c, &new)).await;
    let id = match created {
        Ok(id) => id,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    let e = Entry::new(&actor, Via::Build, Action::Run, format!("job #{id} queued on {}", server.name))
        .target(format!("job-{id}"));
    activity::write(&state.db, e).await;
    let url = format!("{}{}", state.config.public_url.trim_end_matches('/'), job_url(id));
    Json(json!({"id": id, "url": url, "server": server.name})).into_response()
}

/// May this actor see or stop the job? Its project's agent, the owner, the consultant.
/// May this actor read the job's output? Those who may stop it, and the owner's connected
/// machines, which read everything (`flow builder status`).
fn may_read(actor: &Actor, job: &Job) -> bool {
    matches!(actor, Actor::Device { .. }) || may(actor, job)
}

fn may(actor: &Actor, job: &Job) -> bool {
    match actor {
        Actor::Owner { .. } | Actor::Admin { .. } => true,
        Actor::Project { project_id, .. } => *project_id == job.project_id,
        _ => false,
    }
}

#[derive(Deserialize)]
struct After {
    #[serde(default)]
    after: u64,
}

/// The output from `after` (bytes) on, and how the job stands.
/// `flow remote` following its job: it's still wanted (see remote::ORPHANED_AFTER_MS).
async fn output_api(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    q: Query<After>,
) -> Response {
    state.remote.lock().unwrap_or_else(|e| e.into_inner()).followed.insert(id, now_ms());
    output(State(state), Extension(actor), Path(id), q).await
}

async fn output(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Query(a): Query<After>,
) -> Response {
    let Ok(Some(job)) = state.db.call(move |c| remote::get(c, id)).await else {
        return json_error(StatusCode::NOT_FOUND, "no such job");
    };
    if !may_read(&actor, &job) {
        return json_error(StatusCode::FORBIDDEN, "not your job");
    }
    let (text, next) = remote::read_output(&state.config.data_dir(), id, a.after, 256 * 1024);
    Json(json!({
        "text": text,
        "next": next,
        "state": job.state,
        "reason": job.reason,
        "exit_code": job.exit_code,
        "lines": job.lines,
        "warnings": job.warnings,
        "errors": job.errors,
        "finished": job.is_finished(),
        "time": took(&job),
        // The state chip as the page draws it: its tone and words, so a finished job's chip changes
        // colour too (it kept "running"'s, N-19 #6).
        "chip": job_row(&job).chip,
        "label": job_row(&job).label,
        // What a builder sent back, for flow to fetch (GET …/results/<name>).
        "results": job.artifacts,
    }))
    .into_response()
}

async fn cancel_api(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> Response {
    let Ok(Some(job)) = state.db.call(move |c| remote::get(c, id)).await else {
        return json_error(StatusCode::NOT_FOUND, "no such job");
    };
    if !may(&actor, &job) {
        return json_error(StatusCode::FORBIDDEN, "not your job");
    }
    match remote::cancel(&state, id, &actor.name()).await {
        Ok(()) => Json(json!({"cancelled": id})).into_response(),
        Err(e) => json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn cancel_page(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    actor.require_owner()?;
    remote::cancel(&state, id, "the owner").await?;
    activity::write(
        &state.db,
        Entry::new(&actor, Via::Page, Action::Run, format!("cancelled job #{id}")).target(format!("job-{id}")),
    )
    .await;
    Ok(Redirect::to(&job_url(id)).into_response())
}

/// The page a pause or resume came from, to go back to: one of the Machines pages.
#[derive(Deserialize)]
struct Back {
    #[serde(default)]
    next: String,
}

impl Back {
    fn to(&self) -> &str {
        let n = self.next.as_str();
        let ok = (n == "/machines" || n.starts_with("/machines/") || n.starts_with("/machines?"))
            && !n.contains("//")
            && !n.chars().any(char::is_control);
        if ok { n } else { "/machines" }
    }
}

async fn set_paused(state: &AppState, actor: &Actor, name: &str, paused: bool, back: &Back) -> AppResult<Response> {
    actor.require_owner()?;
    if !remote::servers(state).iter().any(|b| b.name == name) {
        return Err(AppError::NotFound);
    }
    state.remote.lock().unwrap_or_else(|e| e.into_inner()).servers.entry(name.to_owned()).or_default().paused = paused;
    let what = if paused { format!("paused builds on {name}") } else { format!("resumed builds on {name}") };
    activity::write(&state.db, Entry::new(actor, Via::Page, Action::Run, what)).await;
    Ok(Redirect::to(back.to()).into_response())
}

async fn pause(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    axum::Form(back): axum::Form<Back>,
) -> AppResult<Response> {
    set_paused(&state, &actor, &name, true, &back).await
}

async fn resume(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    axum::Form(back): axum::Form<Back>,
) -> AppResult<Response> {
    set_paused(&state, &actor, &name, false, &back).await
}

// ---------------------------------------------------------------------------------------------
// The Machine page's section

pub struct Meter {
    pub label: &'static str,
    pub you: f64,
    pub builds: f64,
    pub total: f64,
    pub unit: &'static str,
}

impl Meter {
    fn pct(&self, v: f64) -> String {
        if self.total <= 0.0 { "0".into() } else { format!("{:.1}", (v / self.total * 100.0).clamp(0.0, 100.0)) }
    }
    pub fn you_pct(&self) -> String {
        self.pct(self.you)
    }
    pub fn builds_pct(&self) -> String {
        self.pct(self.builds)
    }
    pub fn you_s(&self) -> String {
        num(self.you)
    }
    pub fn builds_s(&self) -> String {
        num(self.builds)
    }
    pub fn total_s(&self) -> String {
        num(self.total)
    }
    pub fn free(&self) -> String {
        num((self.total - self.you - self.builds).max(0.0))
    }
}

fn num(v: f64) -> String {
    if v >= 10.0 || v.fract() == 0.0 { format!("{v:.0}") } else { format!("{v:.1}") }
}

pub struct JobRow {
    pub id: i64,
    pub project: String,
    pub command: String,
    pub chip: &'static str,
    pub label: String,
    pub time: String,
    pub sent: String,
    /// Seconds it ran and bytes sent, to sort by (-1: not yet).
    pub v: (i64, i64),
}

pub struct ServerView {
    pub name: String,
    pub online: bool,
    pub paused: bool,
    pub line: String,
    pub meters: Vec<Meter>,
    pub jobs: Vec<JobRow>,
    pub min_threads: String,
    pub min_memory: String,
    /// "3 projects, 96 GB (imaginarium 48, vinculum 31) and the shared cache 22 GB"
    pub folders: String,
    pub keep_days: i64,
}

#[derive(Template)]
#[template(path = "builds.html")]
pub struct Section {
    pub servers: Vec<ServerView>,
    pub is_owner: bool,
}

fn took(j: &Job) -> String {
    let Some(s) = j.started_at else { return "–".into() };
    let end = j.finished_at.unwrap_or_else(now_ms);
    let secs = (end - s) / 1000;
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

fn bytes(n: Option<i64>) -> String {
    match n {
        None => "–".into(),
        Some(b) if b < 1024 => format!("{b} B"),
        Some(b) if b < 1024 * 1024 => format!("{} KB", b / 1024),
        Some(b) => format!("{:.1} MB", b as f64 / 1048576.0),
    }
}

pub fn job_row(j: &Job) -> JobRow {
    let label = match j.state.as_str() {
        "running" => match j.threads {
            Some(t) if j.reason.is_empty() => format!("running · {t} threads"),
            _ if !j.reason.is_empty() => format!("running · {}", j.reason),
            _ => "running".into(),
        },
        "paused" => format!("paused · {}", j.reason),
        "queued" => j.reason.clone(),
        "done" => "done".into(),
        "cancelled" => "cancelled".into(),
        _ => {
            if j.reason.is_empty() {
                "failed".into()
            } else {
                format!("failed · {}", j.reason)
            }
        }
    };
    let chip =
        super::tone::tone(if matches!(j.state.as_str(), "running" | "paused" | "queued" | "done" | "cancelled") {
            &j.state
        } else {
            "failed"
        });
    JobRow {
        id: j.id,
        project: j.project.clone(),
        command: j.command_line(),
        chip,
        label,
        time: took(j),
        sent: bytes(j.sent_bytes),
        v: (
            j.started_at.map(|s| (j.finished_at.unwrap_or_else(now_ms) - s) / 1000).unwrap_or(-1),
            j.sent_bytes.unwrap_or(-1),
        ),
    }
}

pub async fn section_data(state: &AppState, actor: &Actor) -> anyhow::Result<Section> {
    state.remote.lock().unwrap_or_else(|e| e.into_inner()).watched_at = now_ms();
    let jobs = state.db.call(|c| remote::recent(c, None, 40)).await?;
    let now = now_ms();
    let list = remote::servers(state);
    let live = state.remote.lock().unwrap_or_else(|e| e.into_inner());
    let servers = list
        .iter()
        .map(|b| {
            let l = live.servers.get(&b.name);
            let p = l.map(|l| l.probe.clone()).unwrap_or_default();
            let mut facts = Vec::new();
            if !p.model.is_empty() {
                facts.push(p.model.replace(" 16-Core Processor", ""));
            }
            if p.threads > 0.0 {
                facts.push(format!("{} threads", p.threads));
                facts.push(format!("{:.0} GB", p.mem_total_gb));
            }
            if !p.gpu_name.is_empty() {
                facts.push(p.gpu_name.replace("NVIDIA GeForce ", ""));
            }
            facts.push(match (p.at, p.online) {
                (0, _) => "not checked yet".into(),
                (t, true) => format!("online, checked {}", ago(t, now)),
                (t, false) => format!("offline (checked {}): {}", ago(t, now), p.error),
            });
            let mut meters = Vec::new();
            if p.online && p.threads > 0.0 {
                meters.push(Meter {
                    label: "CPU · threads",
                    you: p.yours(),
                    builds: p.builds,
                    total: p.threads,
                    unit: "threads",
                });
                meters.push(Meter {
                    label: "Memory · GB",
                    you: p.mem_yours_gb(),
                    builds: p.mem_builds_gb,
                    total: p.mem_total_gb,
                    unit: "GB",
                });
                if !p.gpu_name.is_empty() {
                    meters.push(Meter { label: "GPU · %", you: p.gpu_util, builds: 0.0, total: 100.0, unit: "%" });
                }
            }
            ServerView {
                name: b.name.clone(),
                online: crate::builders::hub().is_online(&b.name),
                paused: l.is_some_and(|l| l.paused),
                line: facts.join(" · "),
                meters,
                jobs: jobs.iter().filter(|j| j.server == b.name).take(12).map(job_row).collect(),
                folders: l.map(|l| folders(&l.sizes)).unwrap_or_default(),
                keep_days: b.keep_days,
                min_threads: num(b.min_threads),
                min_memory: num(b.min_memory_gb),
            }
        })
        .collect();
    Ok(Section { servers, is_owner: actor.is_owner() })
}

fn folders(sizes: &[(String, f64)]) -> String {
    let projects: Vec<&(String, f64)> = sizes.iter().filter(|(n, _)| n != ".sccache").collect();
    if projects.is_empty() {
        return String::new();
    }
    let total: f64 = projects.iter().map(|(_, g)| g).sum();
    let mut list: Vec<&&(String, f64)> = projects.iter().collect();
    list.sort_by(|a, b| b.1.total_cmp(&a.1));
    let each = list.iter().map(|(n, g)| format!("{n} {}", num(*g))).collect::<Vec<_>>().join(", ");
    let cache =
        sizes.iter().find(|(n, _)| n == ".sccache").map(|(_, g)| format!("; the shared compiler cache {} GB", num(*g)));
    format!(
        "{} project{}, {} GB ({each}){}",
        projects.len(),
        if projects.len() == 1 { "" } else { "s" },
        num(total),
        cache.unwrap_or_default()
    )
}

/// The section alone, for the Machine page to refresh every few seconds.
async fn section(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> AppResult<Response> {
    actor.require_owner()?;
    let s = section_data(&state, &actor).await?;
    Ok(axum::response::Html(s.render().map_err(anyhow::Error::from)?).into_response())
}

// ---------------------------------------------------------------------------------------------
// A job's page

/// A job's page (D-275): the note page's layout, its name as the title, its output in the same
/// numbered lines as a note's log, its facts beside it.
#[derive(Template)]
#[template(path = "remote_job.html")]
struct JobPage {
    nav: Nav,
    path: Vec<super::pages::PathPart>,
    job: Job,
    row: JobRow,
    /// Its name, or its command for a job from before names (then shown as code).
    title: String,
    named: bool,
    /// The project it belongs to: (name, its page).
    project: (String, String),
    /// "imaginarium's project agent"
    by: String,
    started: String,
    facts: Vec<(String, String)>,
    /// (number, the line's class in the code view, the line)
    lines: Vec<(usize, &'static str, String)>,
    next: u64,
    is_owner: bool,
}

/// A line's class on the Log: AgentFlow's own notes dimmed, errors and warnings marked.
pub fn line_class(line: &str) -> &'static str {
    if line.starts_with("· ") {
        "d"
    } else {
        match remote::line_severity(line) {
            "error" => "e",
            "warn" => "w",
            _ => "",
        }
    }
}

/// The same in the code view (a job's page, as a note's log): the library's tones.
pub fn cv_class(line: &str) -> &'static str {
    match line_class(line) {
        "d" => "cv-dim",
        "e" => "lv-error",
        "w" => "lv-warn",
        _ => "",
    }
}

async fn job_page(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    tz: crate::tz::Tz,
) -> AppResult<Response> {
    actor.require_owner()?;
    let job = state.db.call(move |c| remote::get(c, id)).await?.ok_or(AppError::NotFound)?;
    // The last part of the output; the page follows from there.
    let path = remote::log_path(&state.config.data_dir(), id);
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(512 * 1024);
    let (mut output, next) = remote::read_output(&state.config.data_dir(), id, from, 512 * 1024);
    if from > 0
        && let Some(i) = output.find('\n')
    {
        output = format!("· … the start is in the Log\n{}", &output[i + 1..]);
    }
    let mut facts = vec![("Server".into(), job.server.clone()), ("Checkout".into(), job.dir.clone())];
    if !job.name.is_empty() {
        facts.insert(0, ("Command".into(), job.command_line()));
    }
    if !job.subdir.is_empty() {
        facts.push(("Runs in".into(), job.subdir.clone()));
    }
    if !job.commit_sha.is_empty() {
        facts.push((
            "Commit".into(),
            format!(
                "{}{}",
                job.commit_sha.get(..8).unwrap_or(""),
                if job.dirty > 0 { format!(" + {} changed files", job.dirty) } else { String::new() }
            ),
        ));
    }
    facts.push(("Sent".into(), bytes(job.sent_bytes)));
    if let Some(t) = job.threads {
        facts.push(("Threads at the start".into(), t.to_string()));
    }
    let n = |k: i64, one: &str, many: &str| format!("{k} {}", if k == 1 { one } else { many });
    facts.push((
        "Output".into(),
        format!(
            "{}, {}, {}",
            n(job.lines, "line", "lines"),
            n(job.warnings, "warning", "warnings"),
            n(job.errors, "error", "errors")
        ),
    ));
    if !job.fetch.is_empty() {
        facts.push(("Brings back".into(), job.fetch.join(", ")));
    }
    if job.gpu {
        facts.push(("GPU".into(), "waits for an idle GPU".into()));
    }
    use super::pages::PathPart;
    let path = vec![
        PathPart::link("Machines", "/machines"),
        PathPart::link("Build servers", "/machines#builds"),
        PathPart::here(format!("Job #{}", job.id)),
    ];
    let pid = job.project_id;
    let pname = state.db.call(move |c| crate::projects::get_by_id(c, pid).map(|p| p.name)).await.unwrap_or_default();
    let by = if pname.is_empty() {
        format!("the {} agent", job.project)
    } else if pname.ends_with('s') {
        format!("{pname}' project agent")
    } else {
        format!("{pname}'s project agent")
    };
    render(JobPage {
        nav: nav(&state, &actor, "/machines").await.up(&path),
        path,
        row: job_row(&job),
        title: job.title(),
        project: (if pname.is_empty() { job.project.clone() } else { pname.clone() }, format!("/p/{}", job.project)),
        named: !job.name.is_empty(),
        by,
        started: tz.short(job.created_at),
        facts,
        lines: output.lines().enumerate().map(|(i, l)| (i + 1, cv_class(l), l.to_owned())).collect(),
        next,
        is_owner: actor.is_owner(),
        job,
    })
}

// ---------------------------------------------------------------------------------------------
// The Settings page's Build servers card: the builders, their limits

pub struct ServerSetting {
    pub name: String,
    /// No token yet: waits for `flow builder add` on its machine (a former SSH builder).
    pub waiting: bool,
    pub online: bool,
    /// Under the name: "online · builder 0.0.0-dev+g92b2be2", "offline · last seen 2 h ago".
    pub state: String,
    /// "32 threads · 92 GB · RTX 5090"
    pub machine: String,
    pub running: usize,
    pub queued: usize,
    /// Expanded: (label, value, detail).
    pub facts: Vec<(&'static str, String, String)>,
    /// Its jobs now: (id, "imaginarium · cargo test", state, "running 2m 14s · 14 threads · slot 1").
    pub jobs: Vec<(i64, String, String, String)>,
    pub min_threads: String,
    pub min_memory: String,
    pub reserve_memory: String,
    pub keep_days: i64,
    pub slots: i64,
    pub projects: Vec<String>,
}

#[derive(Template)]
#[template(path = "build_servers_settings.html")]
pub struct ServersCard {
    pub servers: Vec<ServerSetting>,
    pub notice: Option<(bool, String)>,
}

/// Messages after the card's buttons; the query names one, never free text.
fn card_notice(code: &str) -> Option<(bool, String)> {
    Some(match code {
        "removed" => (true, "Removed. Projects that used it build where their agent is.".into()),
        "saved" => (true, "Limits saved.".into()),
        "renamed" => (true, "Renamed. It reconnects under the new name.".into()),
        c if c.starts_with("error-") => (
            false,
            match c {
                "error-name" => "That name is taken, or isn't letters, digits, - _ .".into(),
                "error-busy" => "It has jobs waiting or running; cancel them first.".into(),
                "error-limits" => "The limits can't be negative, and folders are kept at least a day.".into(),
                _ => "That didn't work; see the Log.".into(),
            },
        ),
        _ => return None,
    })
}

pub async fn servers_card(state: &AppState, code: Option<&str>) -> anyhow::Result<String> {
    let list = remote::servers(state);
    let (picks, with_token): (Vec<(String, String)>, Vec<String>) = state
        .db
        .call(|c| {
            let mut st = c.prepare("SELECT b.server, p.name FROM project_build_server b JOIN projects p ON p.id = b.project_id ORDER BY p.name")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            let mut st = c.prepare("SELECT DISTINCT name FROM tokens WHERE kind = 'builder' AND revoked_at IS NULL")?;
            let names = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            Ok((rows, names))
        })
        .await?;
    let now = now_ms();
    let names: Vec<String> = list.iter().map(|b| b.name.clone()).collect();
    let jobs: Vec<Vec<Job>> = state
        .db
        .call(move |c| names.iter().map(|n| remote::in_state(c, n, &["queued", "running", "paused"])).collect())
        .await?;
    let hub = crate::builders::hub();
    let live = state.remote.lock().unwrap_or_else(|e| e.into_inner());
    let servers = list
        .iter()
        .zip(jobs)
        .map(|(b, jobs)| {
            let l = live.servers.get(&b.name);
            let p = l.map(|l| l.probe.clone()).unwrap_or_default();
            let waiting = !with_token.contains(&b.name);
            let online = hub.is_online(&b.name);
            let hello = hub.hello(&b.name);
            let state = if waiting {
                "former SSH builder".to_owned()
            } else if online {
                match &hello {
                    Some((v, ..)) => format!("online · builder {v}"),
                    None => "online".to_owned(),
                }
            } else if p.at > 0 {
                format!("offline · last seen {}", ago(p.at, now))
            } else {
                "not connected".to_owned()
            };
            let gpu = p.gpu_name.replace("NVIDIA GeForce ", "");
            let machine = if p.threads > 0.0 {
                let mut m = format!("{} threads · {:.0} GB", p.threads, p.mem_total_gb);
                if !gpu.is_empty() {
                    m.push_str(&format!(" · {gpu}"));
                }
                m
            } else if waiting {
                "not connected yet".to_owned()
            } else {
                String::new()
            };
            let mut facts = Vec::new();
            if p.threads > 0.0 {
                facts.push((
                    "CPU",
                    format!("{} · {} threads", p.model.replace(" 16-Core Processor", ""), p.threads),
                    format!(
                        "{:.0}% busy: you {:.0} threads, builds {:.0}",
                        100.0 * p.busy / p.threads.max(1.0),
                        p.yours(),
                        p.builds
                    ),
                ));
                facts.push((
                    "Memory",
                    format!("{:.0} of {:.0} GB used", p.mem_total_gb - p.mem_free_gb, p.mem_total_gb),
                    format!("builds {:.0} GB", p.mem_builds_gb),
                ));
                if !p.gpu_name.is_empty() {
                    facts.push(("GPU", p.gpu_name.clone(), format!("{:.0}%", p.gpu_util)));
                }
                if p.disk_total_gb > 0.0 {
                    let folders: f64 = l.map(|l| l.sizes.iter().map(|(_, g)| g).sum()).unwrap_or(0.0);
                    facts.push((
                        "Disk",
                        format!("{:.0} of {:.0} GB free", p.disk_free_gb, p.disk_total_gb),
                        if folders > 0.0 { format!("work folders {folders:.0} GB") } else { String::new() },
                    ));
                }
            }
            if let Some((v, ..)) = &hello {
                facts.push(("Builder", v.clone(), String::new()));
            }
            if let Some(t) = hub.since(&b.name) {
                facts.push(("Online", format!("for {}", ago(t, now).trim_end_matches(" ago")), String::new()));
            }
            let job_rows = jobs
                .iter()
                .map(|j| {
                    let mut sub = Vec::new();
                    match (j.state.as_str(), j.started_at) {
                        ("running", Some(t)) => sub.push(format!("running {}", took_since(t, now))),
                        _ if !j.reason.is_empty() => sub.push(j.reason.clone()),
                        _ => sub.push(format!("asked {}", ago(j.created_at, now))),
                    }
                    if let Some(t) = j.threads.filter(|_| j.state == "running") {
                        sub.push(format!("{t} threads"));
                    }
                    if b.slots > 1 && j.state == "running" {
                        sub.push(format!("slot {}", j.slot));
                    }
                    let state = if j.state == "queued" { "waiting".to_owned() } else { j.state.clone() };
                    (j.id, format!("{} · {}", j.project, j.command.join(" ")), state, sub.join(" · "))
                })
                .collect();
            ServerSetting {
                name: b.name.clone(),
                waiting,
                online,
                state,
                machine,
                running: jobs.iter().filter(|j| j.state != "queued").count(),
                queued: jobs.iter().filter(|j| j.state == "queued").count(),
                facts,
                jobs: job_rows,
                min_threads: num(b.min_threads),
                min_memory: num(b.min_memory_gb),
                reserve_memory: num(b.reserve_memory_gb),
                keep_days: b.keep_days,
                slots: b.slots,
                projects: picks.iter().filter(|(s, _)| s == &b.name).map(|(_, n)| n.clone()).collect(),
            }
        })
        .collect();
    drop(live);
    Ok(ServersCard { servers, notice: code.and_then(card_notice) }.render()?)
}

/// "2m 14s", "1h 05m" since `t`.
fn took_since(t: i64, now: i64) -> String {
    let s = ((now - t) / 1000).max(0);
    if s >= 3600 { format!("{}h {:02}m", s / 3600, s % 3600 / 60) } else { format!("{}m {:02}s", s / 60, s % 60) }
}

fn back_to_card(code: &str) -> Response {
    Redirect::to(&format!("/settings?s=builders&bs={code}")).into_response()
}

fn find_server(state: &AppState, name: &str) -> AppResult<crate::config::BuildServer> {
    remote::servers(state).into_iter().find(|b| b.name == name).ok_or(AppError::NotFound)
}

#[derive(Deserialize)]
pub struct Limits {
    min_threads: f64,
    min_memory_gb: f64,
    reserve_memory_gb: f64,
    keep_days: i64,
    #[serde(default = "one")]
    slots: i64,
}

fn one() -> i64 {
    1
}

pub async fn save_limits(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    axum::Form(f): axum::Form<Limits>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let mut b = find_server(&state, &name)?;
    (b.min_threads, b.min_memory_gb, b.reserve_memory_gb, b.keep_days, b.slots) =
        (f.min_threads, f.min_memory_gb, f.reserve_memory_gb, f.keep_days, f.slots);
    let n = name.clone();
    if state.db.call(move |c| remote::set_limits(c, &n, &b)).await.is_err() {
        return Ok(back_to_card("error-limits"));
    }
    remote::reload(&state).await;
    Ok(back_to_card("saved"))
}

#[derive(serde::Deserialize)]
pub struct Rename {
    name: String,
}

/// The owner renames a builder on the page (the owner, 2026-11-13: "in the UI i can still rename
/// and remove builders").
pub async fn rename_server(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    axum::Form(f): axum::Form<Rename>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let new = f.name.trim().to_owned();
    if new == name {
        return Ok(back_to_card("saved"));
    }
    let (old, n2) = (name.clone(), new.clone());
    if state.db.call(move |c| remote::rename_server(c, &old, &n2)).await.is_err() {
        return Ok(back_to_card("error-name"));
    }
    crate::builders::hub().drop_builder(&name);
    remote::reload(&state).await;
    activity::write(
        &state.db,
        Entry::new(&actor, Via::Page, Action::Write, format!("builder {name} renamed to {new}")),
    )
    .await;
    Ok(back_to_card("renamed"))
}

pub async fn remove_server(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
) -> AppResult<Response> {
    actor.require_owner()?;
    find_server(&state, &name)?;
    let n = name.clone();
    if state.db.call(move |c| remote::remove_server(c, &n)).await.is_err() {
        return Ok(back_to_card("error-busy"));
    }
    crate::builders::hub().drop_builder(&name);
    remote::reload(&state).await;
    state.remote.lock().unwrap_or_else(|e| e.into_inner()).servers.remove(&name);
    activity::write(&state.db, Entry::new(&actor, Via::Page, Action::Write, format!("removed build server {name}")))
        .await;
    Ok(back_to_card("removed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An online builder's meters are the build-server meter (.bmeter, its own stacked bar), and
    /// their key is the library's legend with its swatches (PR 10: .key and the meter's old name
    /// clashed with the library's .meter).
    #[test]
    fn an_online_builder_shows_its_meters_and_their_legend() {
        let html = Section {
            servers: vec![ServerView {
                name: "headquarters".into(),
                online: true,
                paused: false,
                line: "online".into(),
                meters: vec![Meter { label: "CPU · threads", you: 4.0, builds: 8.0, total: 32.0, unit: "threads" }],
                jobs: vec![],
                min_threads: "8".into(),
                min_memory: "16".into(),
                folders: String::new(),
                keep_days: 14,
            }],
            is_owner: true,
        }
        .render()
        .unwrap();
        assert!(
            html.contains(r#"<div class="bmeters">"#)
                && html.contains(r#"<div class="bmeter"><span class="label">CPU · threads</span>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<ul class="legend"><li><span class="swatch" style="--c: var(--bar-you)"></span>you"#),
            "{html}"
        );
        assert!(!html.contains(r#"class="meter""#) && !html.contains(r#"class="key""#), "{html}");
    }

    /// Build servers start folded, even a lone one (the owner, 2026-11-23).
    #[test]
    fn a_lone_builder_starts_folded() {
        let server = ServerSetting {
            name: "HEADQUARTERS".into(),
            waiting: false,
            online: true,
            state: "online · builder 0.0.0-dev".into(),
            machine: "32 threads · 92 GB".into(),
            running: 1,
            queued: 0,
            facts: Vec::new(),
            jobs: Vec::new(),
            min_threads: String::new(),
            min_memory: String::new(),
            reserve_memory: String::new(),
            keep_days: 7,
            slots: 1,
            projects: Vec::new(),
        };
        let html = ServersCard { servers: vec![server], notice: None }.render().unwrap();
        assert!(
            html.contains(r#"<details class="sl-item">"#) && !html.contains(r#"<details class="sl-item" open>"#),
            "{html}"
        );
        // The limits' number fields carry our own steppers.
        assert_eq!(html.matches(r#"<span class="numf"><input"#).count(), 5, "{html}");
        assert_eq!(html.matches(r#"data-step="-1" aria-label="One less""#).count(), 5);
    }
}
