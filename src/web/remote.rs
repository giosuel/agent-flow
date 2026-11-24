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
        .route("/api/remote/jobs/{id}/output", get(output))
        .route("/api/remote/jobs/{id}/cancel", post(cancel_api))
        .route("/machine/builds", get(section))
        .route("/machine/jobs/{id}", get(job_page))
        .route("/machine/jobs/{id}/output", get(output))
        .route("/machine/jobs/{id}/cancel", post(cancel_page))
        .route("/machine/servers/{name}/pause", post(pause))
        .route("/machine/servers/{name}/resume", post(resume))
        .route("/settings/build-servers", post(add_server))
        .route("/settings/build-servers/{name}/check", post(check_server))
        .route("/settings/build-servers/{name}/setup/{step}", post(setup_step))
        .route("/settings/build-servers/{name}/limits", post(save_limits))
        .route("/settings/build-servers/{name}/remove", post(remove_server))
}

#[derive(Deserialize)]
struct StartJob {
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
        // Nothing to build on: like offline, so the agent builds here.
        let why = match wanted {
            Some(n) => format!("there's no build server called {n}; build here instead"),
            None => "this project has no build server (the owner picks one in its Settings); build here instead".into(),
        };
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": why, "offline": true}))).into_response();
    };
    let online = if server.kind == "daemon" {
        Some(crate::builders::hub().is_online(&server.name))
    } else {
        state.remote.lock().unwrap_or_else(|e| e.into_inner()).servers.get(&server.name).map(|l| l.probe.online)
    };
    if online == Some(false) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": format!("{} is offline; build here instead", server.name), "offline": true})),
        )
            .into_response();
    }
    // A builder that connects itself takes the snapshot; an SSH one reads the checkout here.
    let new = if s.setup || server.kind == "daemon" {
        // From anywhere: what runs comes with the job, the server reads no checkout.
        if !s.setup && s.manifest.is_empty() {
            return json_error(StatusCode::BAD_REQUEST, "this builder takes a snapshot of your checkout: update flow and run flow remote again");
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
    } else {
        // The SSH path: the checkout on this machine. Its top level and where in it to run.
        let dir = std::path::Path::new(&s.dir);
        if !dir.is_absolute() || !dir.is_dir() {
            return json_error(StatusCode::BAD_REQUEST, "dir is the absolute path of a folder in your checkout");
        }
        let top =
            tokio::process::Command::new("git").arg("-C").arg(dir).args(["rev-parse", "--show-toplevel"]).output().await;
        let top = match top {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
            _ => return json_error(StatusCode::BAD_REQUEST, "dir isn't in a git checkout"),
        };
        let subdir = dir
            .canonicalize()
            .ok()
            .and_then(|d| d.strip_prefix(&top).ok().map(|p| p.to_string_lossy().to_string()))
            .unwrap_or_default();
        NewJob {
            server: server.name.clone(),
            project_id: *project_id,
            actor: slug.clone(),
            dir: top,
            subdir,
            command: s.command,
            fetch: s.fetch,
            gpu: s.gpu,
            ..Default::default()
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
    let url = format!("{}/machine/jobs/{id}", state.config.public_url.trim_end_matches('/'));
    Json(json!({"id": id, "url": url, "server": server.name})).into_response()
}

/// May this actor see or stop the job? Its project's agent, the owner, the consultant.
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
async fn output(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Query(a): Query<After>,
) -> Response {
    let Ok(Some(job)) = state.db.call(move |c| remote::get(c, id)).await else {
        return json_error(StatusCode::NOT_FOUND, "no such job");
    };
    if !may(&actor, &job) {
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
    Ok(Redirect::to(&format!("/machine/jobs/{id}")).into_response())
}

async fn set_paused(state: &AppState, actor: &Actor, name: &str, paused: bool) -> AppResult<Response> {
    actor.require_owner()?;
    if !remote::servers(state).iter().any(|b| b.name == name) {
        return Err(AppError::NotFound);
    }
    state.remote.lock().unwrap_or_else(|e| e.into_inner()).servers.entry(name.to_owned()).or_default().paused = paused;
    let what = if paused { format!("paused builds on {name}") } else { format!("resumed builds on {name}") };
    activity::write(&state.db, Entry::new(actor, Via::Page, Action::Run, what)).await;
    Ok(Redirect::to("/machine#builds").into_response())
}

async fn pause(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
) -> AppResult<Response> {
    set_paused(&state, &actor, &name, true).await
}

async fn resume(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
) -> AppResult<Response> {
    set_paused(&state, &actor, &name, false).await
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
}

pub struct ServerView {
    pub name: String,
    pub ssh: String,
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
    let (chip, label) = match j.state.as_str() {
        "running" => (
            "run",
            match j.threads {
                Some(t) if j.reason.is_empty() => format!("running · {t} threads"),
                _ if !j.reason.is_empty() => format!("running · {}", j.reason),
                _ => "running".into(),
            },
        ),
        "paused" => ("warn", format!("paused · {}", j.reason)),
        "queued" => ("quiet", j.reason.clone()),
        "done" => ("ok", "done".into()),
        "cancelled" => ("quiet", "cancelled".into()),
        _ => ("bad", if j.reason.is_empty() { "failed".into() } else { format!("failed · {}", j.reason) }),
    };
    JobRow {
        id: j.id,
        project: j.project.clone(),
        command: j.command_line(),
        chip,
        label,
        time: took(j),
        sent: bytes(j.sent_bytes),
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
                ssh: b.ssh.clone(),
                online: p.online,
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

#[derive(Template)]
#[template(path = "remote_job.html")]
struct JobPage {
    nav: Nav,
    job: Job,
    row: JobRow,
    command: String,
    started: String,
    facts: Vec<(String, String)>,
    lines: Vec<(&'static str, String)>,
    next: u64,
    is_owner: bool,
}

/// A line's class on the page: AgentFlow's own notes dimmed, errors and warnings marked.
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
    facts.push(("Output".into(), format!("{} lines, {} warnings, {} errors", job.lines, job.warnings, job.errors)));
    if !job.fetch.is_empty() {
        facts.push(("Brings back".into(), job.fetch.join(", ")));
    }
    if job.gpu {
        facts.push(("GPU".into(), "waits for an idle GPU".into()));
    }
    let started = format!("started by the {} agent {}", job.project, tz.full(job.created_at).get(..16).unwrap_or(""));
    render(JobPage {
        nav: nav(&state, &actor, "/machine").await.back_to("/machine#builds", "Machine"),
        row: job_row(&job),
        command: job.command_line(),
        started,
        facts,
        lines: output.lines().map(|l| (line_class(l), l.to_owned())).collect(),
        next,
        is_owner: actor.is_owner(),
        job,
    })
}

// ---------------------------------------------------------------------------------------------
// The Settings page's Build servers card: add a machine, its key, its tools, its limits

pub struct ServerSetting {
    pub name: String,
    pub ssh: String,
    pub online: bool,
    pub line: String,
    pub cargo: String,
    pub sccache: String,
    pub checked: bool,
    pub setup: String,
    pub setup_running: bool,
    /// The line to add to the build user's authorized_keys there, when AgentFlow made the key.
    pub key_line: Option<String>,
    pub min_threads: String,
    pub min_memory: String,
    pub reserve_memory: String,
    pub keep_days: i64,
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
        "added" => (
            true,
            "Added. If AgentFlow made a key for it, add the line below on that machine; then press Check.".into(),
        ),
        "removed" => (true, "Removed. Projects that used it build where their agent is.".into()),
        "saved" => (true, "Limits saved.".into()),
        "checking" => (true, "Checking; the result shows here in a few seconds (reload).".into()),
        "started" => (true, "Started. It runs in the background; the result shows here and in the Log.".into()),
        c if c.starts_with("error-") => (
            false,
            match c {
                "error-name" => "That name is taken, or isn't letters, digits, - _ .".into(),
                "error-target" => "Connect to user@host, or a Host from ~/.ssh/config.".into(),
                "error-busy" => "It has jobs waiting or running; cancel them first.".into(),
                "error-key" => "Couldn't make a key (is ssh-keygen installed?).".into(),
                "error-limits" => "The limits can't be negative, and folders are kept at least a day.".into(),
                _ => "That didn't work; see the Log.".into(),
            },
        ),
        _ => return None,
    })
}

pub async fn servers_card(state: &AppState, code: Option<&str>) -> anyhow::Result<String> {
    let list = remote::servers(state);
    let picks: Vec<(String, String)> = state
        .db
        .call(|c| {
            let mut st = c.prepare("SELECT b.server, p.name FROM project_build_server b JOIN projects p ON p.id = b.project_id ORDER BY p.name")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            Ok(rows)
        })
        .await?;
    let now = now_ms();
    let live = state.remote.lock().unwrap_or_else(|e| e.into_inner());
    let servers = list
        .iter()
        .map(|b| {
            let l = live.servers.get(&b.name);
            let p = l.map(|l| l.probe.clone()).unwrap_or_default();
            let line = match (p.at, p.online) {
                (0, _) => "not reached yet".to_owned(),
                (t, true) => format!(
                    "online, checked {} · {} · {} threads · {:.0} GB{}",
                    ago(t, now),
                    p.model.replace(" 16-Core Processor", ""),
                    p.threads,
                    p.mem_total_gb,
                    if p.gpu_name.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", p.gpu_name.replace("NVIDIA GeForce ", ""))
                    }
                ),
                (t, false) => format!("can't be reached (checked {}): {}", ago(t, now), p.error),
            };
            let tools = l.map(|l| l.tools.clone()).unwrap_or_default();
            ServerSetting {
                name: b.name.clone(),
                ssh: b.ssh.clone(),
                online: p.online,
                line,
                cargo: tools.cargo.clone(),
                sccache: tools.sccache.clone(),
                checked: l.is_some_and(|l| l.tools_checked()),
                setup: l.map(|l| l.setup.clone()).unwrap_or_default(),
                setup_running: l.is_some_and(|l| l.setup_running),
                key_line: remote::public_key(&b.key_file).map(|k| format!("restrict {k}")),
                min_threads: num(b.min_threads),
                min_memory: num(b.min_memory_gb),
                reserve_memory: num(b.reserve_memory_gb),
                keep_days: b.keep_days,
                projects: picks.iter().filter(|(s, _)| s == &b.name).map(|(_, n)| n.clone()).collect(),
            }
        })
        .collect();
    drop(live);
    Ok(ServersCard { servers, notice: code.and_then(card_notice) }.render()?)
}

fn back_to_card(code: &str) -> Response {
    Redirect::to(&format!("/settings?bs={code}#s-builds")).into_response()
}

#[derive(Deserialize)]
pub struct AddServer {
    name: String,
    ssh: String,
    #[serde(default)]
    make_key: Option<String>,
}

pub async fn add_server(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    axum::Form(f): axum::Form<AddServer>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let mut b = crate::config::BuildServer {
        name: f.name.trim().to_owned(),
        ssh: f.ssh.trim().to_owned(),
        ..Default::default()
    };
    if remote::check_server(&b).is_err() {
        let code = if b.ssh.is_empty()
            || remote::check_server(&crate::config::BuildServer { ssh: "x".into(), ..b.clone() }).is_ok()
        {
            "error-target"
        } else {
            "error-name"
        };
        return Ok(back_to_card(code));
    }
    if remote::servers(&state).iter().any(|s| s.name == b.name) {
        return Ok(back_to_card("error-name"));
    }
    if f.make_key.is_some() {
        match remote::make_key(&state.config.data_dir(), &b.name).await {
            Ok((path, _)) => b.key_file = path,
            Err(e) => {
                tracing::warn!("making a key for {}: {e:#}", b.name);
                return Ok(back_to_card("error-key"));
            }
        }
    }
    let b2 = b.clone();
    if state.db.call(move |c| remote::add_server(c, &b2)).await.is_err() {
        return Ok(back_to_card("error-name"));
    }
    remote::reload(&state).await;
    activity::write(
        &state.db,
        Entry::new(&actor, Via::Page, Action::Write, format!("added build server {} ({})", b.name, b.ssh)),
    )
    .await;
    Ok(back_to_card("added"))
}

fn find_server(state: &AppState, name: &str) -> AppResult<crate::config::BuildServer> {
    remote::servers(state).into_iter().find(|b| b.name == name).ok_or(AppError::NotFound)
}

pub async fn check_server(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let b = find_server(&state, &name)?;
    let st = state.clone();
    tokio::spawn(async move {
        if let Err(e) = remote::refresh_tools(&st, &b).await {
            let mut live = st.remote.lock().unwrap_or_else(|e| e.into_inner());
            live.servers.entry(b.name.clone()).or_default().setup = format!("couldn't connect: {e:#}");
        }
    });
    Ok(back_to_card("checking"))
}

pub async fn setup_step(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((name, step)): Path<(String, String)>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let b = find_server(&state, &name)?;
    let what = match step.as_str() {
        "sccache" => "installing sccache",
        "rust" => "setting up Rust",
        _ => return Err(AppError::NotFound),
    };
    activity::write(&state.db, Entry::new(&actor, Via::Page, Action::Run, format!("{what} on {name}"))).await;
    remote::spawn_setup(state, b, what);
    Ok(back_to_card("started"))
}

#[derive(Deserialize)]
pub struct Limits {
    min_threads: f64,
    min_memory_gb: f64,
    reserve_memory_gb: f64,
    keep_days: i64,
}

pub async fn save_limits(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    axum::Form(f): axum::Form<Limits>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let mut b = find_server(&state, &name)?;
    (b.min_threads, b.min_memory_gb, b.reserve_memory_gb, b.keep_days) =
        (f.min_threads, f.min_memory_gb, f.reserve_memory_gb, f.keep_days);
    let n = name.clone();
    if state.db.call(move |c| remote::set_limits(c, &n, &b)).await.is_err() {
        return Ok(back_to_card("error-limits"));
    }
    remote::reload(&state).await;
    Ok(back_to_card("saved"))
}

pub async fn remove_server(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let b = find_server(&state, &name)?;
    let n = name.clone();
    if state.db.call(move |c| remote::remove_server(c, &n)).await.is_err() {
        return Ok(back_to_card("error-busy"));
    }
    if !b.key_file.is_empty() {
        let _ = tokio::fs::remove_file(&b.key_file).await;
        let _ = tokio::fs::remove_file(format!("{}.pub", b.key_file)).await;
    }
    remote::reload(&state).await;
    state.remote.lock().unwrap_or_else(|e| e.into_inner()).servers.remove(&name);
    activity::write(&state.db, Entry::new(&actor, Via::Page, Action::Write, format!("removed build server {name}")))
        .await;
    Ok(back_to_card("removed"))
}
