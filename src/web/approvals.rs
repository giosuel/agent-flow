//! What `flow` needs from the server besides the agents' API: asking for approvals and polling
//! them (see [`crate::approvals`]), the owner's approval page, the install script and the `flow`
//! download (public: the repository is public anyway), `flow status`, and revoking its own token
//! (`flow disconnect`).

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::pages::{Nav, nav, render};
use super::{AppError, AppResult, AppState, ClientIp, json_error};
use crate::activity::{self, Action, Entry, Via};
use crate::approvals::{self, Approval};
use crate::auth::Actor;
use crate::push::{self, Notification};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/approvals", post(request))
        .route("/api/approvals/poll", post(poll))
        .route("/approve/{code}", get(page).post(decide))
        .route("/api/status", get(status))
        .route("/api/tokens", get(tokens))
        .route("/api/projects", get(projects))
        .route("/api/tokens/self", axum::routing::delete(revoke_self))
        .route("/install.sh", get(install_script))
        .route("/api/setup/redeem", post(redeem))
        .route("/api/setup/done", post(set_up))
        .route("/download/flow.sha256", get(flow_sum))
}

/// Paths anyone may call: asking to connect, polling with the secret only `flow` has, the install
/// script and the `flow` program with its checksum.
pub fn is_public(path: &str) -> bool {
    matches!(
        path,
        "/api/approvals"
            | "/api/approvals/poll"
            | "/api/setup/redeem"
            | "/install.sh"
            | "/download/flow"
            | "/download/agentflow"
            | "/download/flow.sha256"
    )
}

#[derive(Deserialize)]
struct RequestBody {
    action: String,
    #[serde(default)]
    args: Value,
    machine: String,
}

/// `flow` asks for something. Connecting needs no token (that's how a machine gets one); the
/// owner's admin jobs come from a connected machine (a device token) and still need approval.
async fn request(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Json(body): Json<RequestBody>,
) -> Response {
    if !approvals::ACTIONS.contains(&body.action.as_str()) {
        return json_error(StatusCode::BAD_REQUEST, format!("unknown request {:?}", body.action));
    }
    let requested_by = match (&actor, body.action.as_str()) {
        (_, "connect") => match &actor {
            Actor::Anonymous => String::new(),
            other => other.name(),
        },
        (Actor::Device { name, .. }, _) => name.clone(),
        _ => {
            return json_error(
                StatusCode::FORBIDDEN,
                "this needs a connected machine: run `flow connect <server>` first (agents' tokens can't ask for it)",
            );
        }
    };
    let args = if body.args.is_null() { json!({}) } else { body.args };
    let (action, machine, by, ip2) = (body.action.clone(), body.machine.clone(), requested_by.clone(), ip.clone());
    let created = state
        .db
        .call(move |c| {
            let made = approvals::create(c, &action, &args, &machine, &by, Some(&ip2))?;
            let a = approvals::by_code(c, &made.0)?;
            Ok((made, a))
        })
        .await;
    let ((code, poll), a) = match created {
        Ok(((code, poll), Some(a))) => ((code, poll), a),
        Ok((_, None)) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "the request vanished"),
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    let what = approvals::describe(&a);
    let e = Entry::new(&actor, Via::Api, Action::Write, format!("approval {code} asked: {what} ({})", a.machine))
        .ip(Some(ip.clone()));
    activity::write(&state.db, e).await;
    let url = format!("{}/approve/{code}", state.config.public_url.trim_end_matches('/'));
    state.push.spawn(
        push::Kind::Review,
        Notification {
            title: format!("Approve? {what}"),
            body: format!("From {} ({ip}). Code {code}.", a.machine),
            url: format!("/approve/{code}"),
            tag: Some(format!("approve-{code}")),
        },
    );
    Json(json!({"code": code, "poll": poll, "url": url, "expires_in": approvals::MINUTES * 60})).into_response()
}

#[derive(Deserialize)]
struct PollBody {
    poll: String,
}

async fn poll(State(state): State<AppState>, Json(body): Json<PollBody>) -> Response {
    match state.db.call(move |c| approvals::poll(c, &body.poll)).await {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => json_error(StatusCode::NOT_FOUND, "no such request"),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[derive(askama::Template)]
#[template(path = "approve.html")]
struct ApprovePage {
    nav: Nav,
    a: Approval,
    what: String,
    details: Vec<(&'static str, String)>,
    asked: String,
    /// After a decision: what came of it.
    outcome: Option<(bool, String)>,
}

fn details(a: &Approval) -> Vec<(&'static str, String)> {
    let mut d = vec![("Machine", a.machine.clone())];
    if let Some(ip) = &a.ip {
        d.push(("From", ip.clone()));
    }
    if !a.requested_by.is_empty() {
        d.push(("Asked by", a.requested_by.clone()));
    }
    if let Some(o) = a.args.as_object() {
        for (k, v) in o {
            let v = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            d.push(("Detail", format!("{k}: {v}")));
        }
    }
    d
}

async fn page(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(code): Path<String>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let a = state.db.call(move |c| approvals::by_code(c, &code)).await?.ok_or(AppError::NotFound)?;
    let now = crate::util::now_ms();
    render(ApprovePage {
        nav: nav(&state, &actor, "/settings").await,
        what: approvals::describe(&a),
        details: details(&a),
        asked: crate::util::ago(a.created_at, now),
        outcome: None,
        a,
    })
}

#[derive(Deserialize)]
struct DecideForm {
    decision: String,
}

async fn decide(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Path(code): Path<String>,
    Form(f): Form<DecideForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let approve = match f.decision.as_str() {
        "approve" => true,
        "deny" => false,
        _ => return Err(AppError::BadRequest("approve or deny".into())),
    };
    let c2 = code.clone();
    let a = match state.db.call(move |c| approvals::decide(c, &c2, approve)).await {
        Ok(a) => a,
        Err(e) => return Err(AppError::BadRequest(format!("{e:#}"))),
    };
    let what = approvals::describe(&a);
    let outcome = if !approve {
        (true, "Denied. Nothing was done.".to_owned())
    } else {
        let done = carry_out(&state, &a).await;
        let line = match &done {
            Ok(_) => "Approved and done. The terminal that asked has it now.".to_owned(),
            Err(e) => format!("Approved, but it failed: {e:#}"),
        };
        let id = a.id;
        let ok = done.is_ok();
        state.db.call(move |c| approvals::finish(c, id, &done)).await?;
        if ok && (a.action == "builder_create" || a.action == "builder_remove") {
            crate::remote::reload(&state).await;
        }
        (ok, line)
    };
    let verb = if approve { "approved" } else { "denied" };
    let e = Entry::new(&actor, Via::Page, Action::Write, format!("approval {code} {verb}: {what} ({})", a.machine))
        .ip(Some(ip));
    activity::write(&state.db, e).await;
    let now = crate::util::now_ms();
    let a2 = state.db.call(move |c| approvals::by_code(c, &code)).await?.ok_or(AppError::NotFound)?;
    render(ApprovePage {
        nav: nav(&state, &actor, "/settings").await,
        what,
        details: details(&a2),
        asked: crate::util::ago(a2.created_at, now),
        outcome: Some(outcome),
        a: a2,
    })
}

/// Does what the owner approved. Importing PRs asks GitHub first, outside the database.
async fn carry_out(state: &AppState, a: &Approval) -> anyhow::Result<Value> {
    if a.action == "import_prs" {
        let s = |k: &str| a.args.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        let n = |k: &str| a.args.get(k).and_then(Value::as_i64).unwrap_or(0);
        let (slug, from, to) = (s("project"), n("from"), n("to"));
        let p = state.db.call(move |c| crate::projects::require(c, &slug)).await?;
        let repo = p.repo.trim().to_owned();
        let r2 = repo.clone();
        let all = tokio::task::spawn_blocking(move || crate::github::all_prs_blocking(&r2, 1000)).await??;
        let wanted: Vec<_> = all.into_iter().filter(|i| (from..=to).contains(&i.number)).collect();
        let (added, linked) = state.db.call(move |c| crate::prs::import(c, &p, &repo, &wanted, "owner")).await?;
        return Ok(json!({"imported": added, "linked": linked}));
    }
    let data = state.config.data_dir();
    let a = a.clone();
    state.db.call(move |c| approvals::execute(c, &a, &data)).await
}

#[derive(Deserialize)]
struct RedeemBody {
    code: String,
    machine: String,
}

/// `flow project setup` with a setup code from the page: the code, once, for a new token of its
/// project. Public: the code is the proof (about 59 bits, 15 minutes, once).
async fn redeem(
    State(state): State<AppState>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Json(b): Json<RedeemBody>,
) -> Response {
    let machine = match approvals::check_machine(&b.machine) {
        Ok(m) => m,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    let m2 = machine.clone();
    match state.db.call(move |c| crate::onboard::redeem_setup_code(c, &b.code, &m2)).await {
        Ok((p, token)) => {
            let e = Entry::new(
                &Actor::Anonymous,
                Via::Api,
                Action::Write,
                format!("setup code used for {} on {machine}", p.slug),
            )
            .project(p.slug.clone())
            .ip(Some(ip));
            activity::write(&state.db, e).await;
            Json(json!({"project": p.slug, "name": p.name, "token": token})).into_response()
        }
        Err(e) => {
            let e2 = Entry::new(&Actor::Anonymous, Via::Api, Action::Denied, format!("setup code refused: {e:#}"))
                .ip(Some(ip));
            activity::write(&state.db, e2).await;
            json_error(StatusCode::FORBIDDEN, format!("{e:#}"))
        }
    }
}

#[derive(Deserialize)]
struct DoneBody {
    folder: String,
    machine: String,
}

/// `flow project setup` reports where the project now lies.
async fn set_up(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(b): Json<DoneBody>,
) -> Response {
    let Actor::Project { project_id, .. } = actor else {
        return json_error(StatusCode::FORBIDDEN, "this needs the project's token");
    };
    match state.db.call(move |c| crate::onboard::mark_set_up(c, project_id, &b.folder, &b.machine)).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

/// `flow status`: the server's version, and who the token belongs to.
async fn status(Extension(actor): Extension<Actor>) -> Response {
    if matches!(actor, Actor::Anonymous) {
        return json_error(StatusCode::UNAUTHORIZED, "sign in or send a token");
    }
    let who = match &actor {
        Actor::Device { name, .. } => json!({"kind": "device", "name": name}),
        Actor::Project { slug, .. } => json!({"kind": "project", "name": slug}),
        Actor::Admin { name, .. } => json!({"kind": "consultant", "name": name}),
        _ => json!({"kind": "owner", "name": "owner"}),
    };
    Json(json!({"version": env!("APP_VERSION"), "who": who})).into_response()
}

/// `flow token list`: every token without its secret. For the owner's machines and the page.
async fn tokens(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> Response {
    if !matches!(actor, Actor::Device { .. } | Actor::Owner { .. }) {
        return json_error(StatusCode::FORBIDDEN, "only your own machines list tokens");
    }
    match state.db.call(|c| crate::auth::list_tokens(c)).await {
        Ok(list) => Json(json!({"tokens": list})).into_response(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// `flow project list`: every project with its folder. Any token may read it.
async fn projects(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> Response {
    if matches!(actor, Actor::Anonymous) {
        return json_error(StatusCode::UNAUTHORIZED, "sign in or send a token");
    }
    match state.db.call(|c| crate::projects::list(c)).await {
        Ok(list) => {
            let rows: Vec<Value> = list
                .iter()
                .map(|p| json!({"slug": p.slug, "name": p.name, "status": p.status, "checkout": p.checkout}))
                .collect();
            Json(json!({"projects": rows})).into_response()
        }
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// `flow disconnect`: a machine revokes its own token, so a copy left on its disk is worthless.
async fn revoke_self(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
) -> Response {
    let Some(id) = actor.token_id() else { return json_error(StatusCode::BAD_REQUEST, "this needs a token") };
    match state.db.call(move |c| crate::auth::revoke_token(c, id)).await {
        Ok(_) => {
            let e = Entry::new(
                &actor,
                Via::Api,
                Action::Write,
                format!("token {id} revoked by its machine (flow disconnect)"),
            )
            .ip(Some(ip));
            activity::write(&state.db, e).await;
            Json(json!({"revoked": id})).into_response()
        }
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// The SHA-256 of the `flow` the server hands out, for the install script and `flow update`.
async fn flow_sum(State(state): State<AppState>) -> Response {
    let path = state.config.flow_bin();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let f = std::fs::File::open(&path)?;
        Ok(crate::builds::sha256_of(f)?.0)
    })
    .await
    {
        Ok(Ok(sum)) => ([(header::CONTENT_TYPE, "text/plain")], format!("{sum}\n")).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// One command to install `flow` (the owner, 2026-11-13: "lets make it a one-command install
/// like wget ... | bash"): `curl -fsSL <server>/install.sh | sh`.
async fn install_script(State(state): State<AppState>) -> Response {
    let url = state.config.public_url.trim_end_matches('/').to_owned();
    let script = INSTALL_SH.replace("@URL@", &url).replace("@GLIBC@", &super::settings::glibc_version());
    ([(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")], script).into_response()
}

const INSTALL_SH: &str = r#"#!/bin/sh
# Installs flow, AgentFlow's command, into ~/.local/bin and connects it to @URL@.
# Run it as the user who'll use flow:  curl -fsSL @URL@/install.sh | sh
set -eu
url='@URL@'
need='@GLIBC@'
say() { printf '%s\n' "$*" >&2; }
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) ;;
  *) say "flow is built for Linux on x86-64 only so far; this is $(uname -s) $(uname -m)."; exit 1 ;;
esac
have=$(ldd --version 2>/dev/null | head -n1 | grep -oE '[0-9]+\.[0-9]+$' || true)
if [ -n "$have" ] && [ -n "$need" ] && [ "$(printf '%s\n%s\n' "$need" "$have" | sort -V | head -n1)" != "$need" ]; then
  say "flow needs glibc $need or newer; this machine has $have."; exit 1
fi
fetch() {
  if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then wget -qO "$2" "$1"
  else say "This needs curl or wget."; exit 1; fi
}
bin="$HOME/.local/bin"
mkdir -p "$bin"
tmp=$(mktemp "$bin/.flow.XXXXXX")
trap 'rm -f "$tmp" "$tmp.sum"' EXIT
fetch "$url/download/flow" "$tmp"
fetch "$url/download/flow.sha256" "$tmp.sum"
sum=$(cut -d' ' -f1 "$tmp.sum")
if [ "$(sha256sum "$tmp" | cut -d' ' -f1)" != "$sum" ]; then say "The download doesn't match its checksum; nothing was installed."; exit 1; fi
chmod 755 "$tmp"
mv "$tmp" "$bin/flow"
trap - EXIT
rm -f "$tmp.sum"
say "Installed $("$bin/flow" --version) in $bin."
case ":$PATH:" in
  *":$bin:"*) ;;
  *) say "~/.local/bin isn't on your PATH yet. fish: fish_add_path ~/.local/bin   bash: echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.bashrc" ;;
esac
# Piped into sh, stdin is the script: connect reads from the terminal, if there is one.
if (exec </dev/tty) 2>/dev/null; then
  exec "$bin/flow" connect "$url" </dev/tty
else
  say "Now connect it: flow connect $url"
fi
"#;
