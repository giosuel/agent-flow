//! End-to-end tests of the HTTP layer: a real server on a random local port, real requests.

use std::net::SocketAddr;

use reqwest::StatusCode;
use reqwest::header::{ACCEPT, CONTENT_TYPE, COOKIE, LOCATION, ORIGIN, SET_COOKIE};
use serde_json::{Value, json};

use super::*;
use crate::auth::TokenKind;

pub struct TestServer {
    pub base: String,
    pub db: Db,
    pub http: reqwest::Client,
    pub github: std::sync::Arc<crate::github::fake::FakeGitHub>,
    /// Stands in for GitHub when the owner sends a draft.
    pub poster: std::sync::Arc<crate::outgoing::fake::FakePoster>,
    pub state: AppState,
    _data: tempfile::TempDir,
}

impl TestServer {
    pub async fn start() -> Self {
        Self::start_with(|_, _| {}).await
    }

    /// With changes to the config (given the test's data folder).
    pub async fn start_with(tweak: impl FnOnce(&mut Config, &std::path::Path)) -> Self {
        let db = Db::open_memory();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Builds and keys go to a folder of the test's own, never the real data directory.
        let data = tempfile::tempdir().unwrap();
        // A handbook of the test's own: a git repository with one guide.
        let handbook = data.path().join("handbook");
        std::fs::create_dir(&handbook).unwrap();
        std::fs::write(handbook.join("MACHINE.md"), "# MACHINE\n\nThe server.\n\n## Ports\n\nUse 10.0.8.1.\n").unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["add", "MACHINE.md"],
            &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"],
        ] {
            assert!(std::process::Command::new("git").arg("-C").arg(&handbook).args(args).status().unwrap().success());
        }
        let config = Config {
            listen: addr,
            public_url: format!("http://{addr}"),
            data_dir: data.path().display().to_string(),
            handbook_dir: handbook.display().to_string(),
            build_free_gb: 0,
            transcripts_dir: data.path().join("transcripts").display().to_string(),
            // "GitHub" is a folder of bare repositories.
            git_host: format!("file://{}", data.path().join("gh").display()),
            ..Config::default()
        };
        let mut config = config;
        tweak(&mut config, data.path());
        let github = std::sync::Arc::new(crate::github::fake::FakeGitHub::default());
        let mut state = AppState::new(db.clone(), config, crate::push::Vapid::random(), github.clone());
        let poster = std::sync::Arc::new(crate::outgoing::fake::FakePoster::default());
        state.poster = poster.clone();
        let app = router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        Self { base: format!("http://{addr}"), db, http, github, poster, state, _data: data }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub async fn token(&self, kind: TokenKind, project: Option<&str>) -> String {
        let project = project.map(str::to_owned);
        self.db
            .call(move |c| {
                if let Some(p) = &project
                    && crate::projects::get(c, p)?.is_none()
                {
                    crate::projects::create(c, p, p, "test")?;
                }
                Ok(auth::create_token(c, kind, "test", project.as_deref())?.1)
            })
            .await
            .unwrap()
    }

    /// Signs in through a one-time link and returns the session cookie.
    pub async fn owner_cookie(&self) -> String {
        let link = self.db.call(|c| auth::create_login_link(c, "signin")).await.unwrap();
        let res = self.http.post(self.url(&format!("/login/link/{link}"))).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let cookie = res.headers()[SET_COOKIE].to_str().unwrap();
        cookie.split(';').next().unwrap().to_owned()
    }

    pub async fn log_summaries(&self) -> Vec<String> {
        self.db
            .call(|c| {
                let mut s = c.prepare("SELECT summary FROM log ORDER BY seq")?;
                let rows = s.query_map([], |r| r.get::<_, String>(0))?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn anonymous_requests_are_refused() {
    let s = TestServer::start().await;
    let res = s.http.get(s.url("/")).header(ACCEPT, "text/html").send().await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[LOCATION], "/login");

    let res = s.http.get(s.url("/api/whoami")).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    let res = s.http.get(s.url("/api/whoami")).bearer_auth("af_dev_nonsense").send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(s.log_summaries().await.iter().any(|l| l.contains("unknown or revoked token")));

    // The sign-in page and its assets are public.
    assert_eq!(s.http.get(s.url("/login")).send().await.unwrap().status(), StatusCode::OK);
    assert_eq!(s.http.get(s.url("/static/app.css")).send().await.unwrap().status(), StatusCode::OK);
    assert_eq!(s.http.get(s.url("/static/nope.css")).send().await.unwrap().status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn tokens_identify_and_are_logged() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let me: Value = s.http.get(s.url("/api/whoami")).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    assert_eq!(me["kind"], "project");
    assert_eq!(me["project"], "vinculum");
    assert!(s.log_summaries().await.contains(&"GET /api/whoami".to_owned()));
    // Asset loads don't flood the log.
    s.http.get(s.url("/static/app.css")).send().await.unwrap();
    assert!(!s.log_summaries().await.iter().any(|l| l.contains("app.css")));
}

#[tokio::test]
async fn login_link_signs_in_once() {
    let s = TestServer::start().await;
    let link = s.db.call(|c| auth::create_login_link(c, "signin")).await.unwrap();
    // Opening the link only shows a confirmation; a link preview can't use it up.
    let page = s.http.get(s.url(&format!("/login/link/{link}"))).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Sign in on this device?"));

    let res = s.http.post(s.url(&format!("/login/link/{link}"))).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let set = res.headers()[SET_COOKIE].to_str().unwrap().to_owned();
    assert!(set.contains("HttpOnly") && set.contains("SameSite=Lax"));
    let cookie = set.split(';').next().unwrap().to_owned();

    let page = s.http.get(s.url("/")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.text().await.unwrap().contains("Dashboard"));

    let again = s.http.post(s.url(&format!("/login/link/{link}"))).send().await.unwrap();
    assert_eq!(again.status(), StatusCode::GONE);
}

/// `sudo flow server add-passkey` (and the first start) hand out a link that leads straight to
/// adding a passkey, logged and announced.
#[tokio::test]
async fn a_passkey_link_leads_to_adding_a_passkey() {
    let s = TestServer::start().await;
    let url = crate::web::admin_socket::passkey_link(&s.state, "root").await.unwrap();
    let link = url.rsplit('/').next().unwrap().to_owned();
    let res = s.http.post(s.url(&format!("/login/link/{link}"))).send().await.unwrap();
    assert_eq!(res.headers()["location"], "/settings?add_passkey=1#passkeys");
    let cookie = res.headers()[SET_COOKIE].to_str().unwrap().split(';').next().unwrap().to_owned();
    let page = s.http.get(s.url("/settings?add_passkey=1")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Add a passkey for this device"), "{page}");
    assert!(s.log_summaries().await.iter().any(|l| l.contains("passkey link made on the server, as root")));
}

#[tokio::test]
async fn cookie_posts_need_our_origin() {
    let s = TestServer::start().await;
    let cookie = s.owner_cookie().await;
    let res = s
        .http
        .post(s.url("/logout"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = s.http.post(s.url("/logout")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "no Origin and no Referer");
    let res = s.http.post(s.url("/logout")).header(COOKIE, &cookie).header(ORIGIN, &s.base).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    // The session is over.
    let res = s.http.get(s.url("/api/whoami")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn security_headers_are_set() {
    let s = TestServer::start().await;
    let res = s.http.get(s.url("/login")).send().await.unwrap();
    let csp = res.headers()["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("script-src 'self'") && csp.contains("frame-ancestors 'none'"));
    assert_eq!(res.headers()["x-content-type-options"], "nosniff");
}

/// Sends one JSON-RPC message to /mcp and returns the JSON response (from JSON or an SSE body).
pub async fn mcp_call(
    s: &TestServer,
    token: &str,
    session: Option<&str>,
    body: Value,
) -> (Option<String>, Option<Value>) {
    let mut req = s
        .http
        .post(s.url("/mcp"))
        .bearer_auth(token)
        .header(ACCEPT, "application/json, text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .header("host", "localhost")
        .json(&body);
    if let Some(id) = session {
        req = req.header("mcp-session-id", id).header("mcp-protocol-version", "2025-06-18");
    }
    let res = req.send().await.unwrap();
    assert!(res.status().is_success(), "MCP {} -> {}", body["method"], res.status());
    let session = res.headers().get("mcp-session-id").map(|v| v.to_str().unwrap().to_owned());
    let text = res.text().await.unwrap();
    let json = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .find(|v| v.get("id").is_some())
        .or_else(|| serde_json::from_str(&text).ok());
    (session, json)
}

/// Opens an MCP session and returns its id.
pub async fn mcp_session(s: &TestServer, token: &str) -> String {
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}});
    let (session, res) = mcp_call(s, token, None, init).await;
    assert_eq!(res.unwrap()["result"]["serverInfo"]["name"], "agentflow");
    let session = session.expect("session id");
    mcp_call(s, token, Some(&session), json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;
    session
}

/// Calls a tool and returns its parsed JSON text (or the error text as a string value).
pub async fn mcp_tool(s: &TestServer, token: &str, session: &str, name: &str, args: Value) -> (bool, Value) {
    let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": name, "arguments": args}});
    let (_, res) = mcp_call(s, token, Some(session), call).await;
    let res = res.expect("a response");
    if let Some(err) = res.get("error") {
        return (false, err.clone());
    }
    let result = &res["result"];
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    let is_error = result["isError"].as_bool().unwrap_or(false);
    (!is_error, serde_json::from_str(text).unwrap_or(Value::String(text.to_owned())))
}

#[tokio::test]
async fn mcp_needs_a_token_and_logs_tool_calls() {
    let s = TestServer::start().await;
    let res = s.http.post(s.url("/mcp")).header("host", "localhost").json(&json!({})).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    let token = s.token(TokenKind::Project, Some("imaginarium")).await;
    let session = mcp_session(&s, &token).await;
    let (ok, me) = mcp_tool(&s, &token, &session, "whoami", json!({})).await;
    assert!(ok);
    assert_eq!(me["project"], "imaginarium");
    assert!(s.log_summaries().await.contains(&"whoami".to_owned()));
}

#[tokio::test]
async fn settings_are_owner_only() {
    let s = TestServer::start().await;
    let admin = s.token(TokenKind::Admin, None).await;
    let res = s.http.get(s.url("/settings")).bearer_auth(&admin).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "not even the consultant");
    let cookie = s.owner_cookie().await;
    let page = s.http.get(s.url("/settings")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.text().await.unwrap().contains("/install.sh | sh"), "the install command is on the page");
}

/// The owner, 2026-11-13: `flow connect` signs a machine in like `gh auth login`; the owner
/// approves in the browser with their passkey; the token reaches flow once and nowhere else.
#[tokio::test]
async fn a_machine_connects_after_the_owner_approves() {
    let s = TestServer::start().await;
    let cookie = s.owner_cookie().await;
    let asked: Value = s
        .http
        .post(s.url("/api/approvals"))
        .json(&json!({"action": "connect", "machine": "viber@mothman"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let code = asked["code"].as_str().unwrap().to_owned();
    let poll = asked["poll"].as_str().unwrap().to_owned();
    assert!(asked["url"].as_str().unwrap().ends_with(&format!("/approve/{code}")));
    let polled = |p: String| {
        let s = &s;
        async move {
            s.http.post(s.url("/api/approvals/poll")).json(&json!({"poll": p})).send().await.unwrap().json::<Value>().await.unwrap()
        }
    };
    assert_eq!(polled(poll.clone()).await["state"], "pending");

    // Only the owner's browser sees and decides it; the consultant's token can't.
    let admin = s.token(TokenKind::Admin, None).await;
    let res = s.http.get(s.url(&format!("/approve/{code}"))).bearer_auth(&admin).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = s.http.post(s.url(&format!("/approve/{code}"))).bearer_auth(&admin).form(&[("decision", "approve")]).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let page = s.http.get(s.url(&format!("/approve/{code}"))).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains(&code) && page.contains("viber@mothman") && page.contains("Approve"), "{page}");
    let settings = s.http.get(s.url("/settings")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(settings.contains("Waiting for your approval") && settings.contains(&code));
    // Signed out, the link leads to sign-in and back.
    let fresh = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let res = fresh.get(s.url(&format!("/approve/{code}"))).header("accept", "text/html").send().await.unwrap();
    assert_eq!(res.headers()["location"], format!("/login?next=/approve/{code}"));

    let done = s
        .http
        .post(s.url(&format!("/approve/{code}")))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("decision", "approve")])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(done.contains("Approved and done"), "{done}");
    let got = polled(poll.clone()).await;
    assert_eq!(got["state"], "approved");
    let token = got["result"]["token"].as_str().unwrap().to_owned();
    let st: Value = s.http.get(s.url("/api/status")).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    assert_eq!(st["who"]["kind"], "device");
    assert_eq!(st["who"]["name"], "viber@mothman");
    assert_eq!(polled(poll).await["state"], "done", "handed out once");
    assert!(!s.log_summaries().await.iter().any(|l| l.contains(&token)), "never in the log");

    // A connected machine asks for an admin job; agents' tokens can't even ask.
    let project = s.token(TokenKind::Project, Some("imaginarium")).await;
    let res = s.http.post(s.url("/api/approvals")).bearer_auth(&project).json(&json!({"action": "backup", "machine": "x"})).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = s.http.post(s.url("/api/approvals")).json(&json!({"action": "backup", "machine": "x"})).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "admin jobs come from a connected machine");
    let asked: Value = s
        .http
        .post(s.url("/api/approvals"))
        .bearer_auth(&token)
        .json(&json!({"action": "token_create", "args": {"kind": "project", "project": "imaginarium"}, "machine": "viber@mothman"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let code = asked["code"].as_str().unwrap().to_owned();
    s.http
        .post(s.url(&format!("/approve/{code}")))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("decision", "deny")])
        .send()
        .await
        .unwrap();
    assert_eq!(polled(asked["poll"].as_str().unwrap().to_owned()).await["state"], "denied");

    // flow disconnect: the machine revokes its own token.
    let res = s.http.delete(s.url("/api/tokens/self")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(s.http.get(s.url("/api/status")).bearer_auth(&token).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);

    // The install script is public and names this server.
    let script = s.http.get(s.url("/install.sh")).send().await.unwrap().text().await.unwrap();
    assert!(script.starts_with("#!/bin/sh") && script.contains(&format!("url='{}'", s.base)) && script.contains("flow\" connect"), "{script}");
}

#[tokio::test]
async fn pwa_files_are_public() {
    let s = TestServer::start().await;
    let m: Value = s.http.get(s.url("/manifest.webmanifest")).send().await.unwrap().json().await.unwrap();
    assert_eq!(m["display"], "standalone");
    let sw = s.http.get(s.url("/sw.js")).send().await.unwrap();
    assert_eq!(sw.headers()["cache-control"], "no-cache");
    assert!(sw.text().await.unwrap().contains("showNotification"));
    assert_eq!(s.http.get(s.url("/static/icons/apple-touch-icon.png")).send().await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn an_agent_runs_its_project_over_mcp() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let other = s.token(TokenKind::Project, Some("imaginarium")).await;
    let sess = mcp_session(&s, &token).await;
    let call = |name: &'static str, args: Value| {
        let (s, token, sess) = (&s, token.clone(), sess.clone());
        async move { mcp_tool(s, &token, &sess, name, args).await }
    };

    let (ok, hello) =
        call("hello", json!({"session": "Vinculum", "machine": "giosuel.ch", "model": "Opus 5.5", "effort": "high"}))
            .await;
    assert!(ok, "{hello}");
    assert_eq!(hello["project"]["agent"]["online"], true);

    assert!(call("add_milestone", json!({"name": "M4", "title": "Keybinds"})).await.0);
    assert!(call("add_milestone", json!({"name": "K1", "title": "Read binds", "step_of": "M4"})).await.0);
    let (ok, t) = call("add_task", json!({"title": "Record a bind", "milestone": "K1", "estimate": "2 h"})).await;
    assert!(ok, "{t}");
    assert_eq!(t["id"], "vinculum/T-1");
    let (ok, t) = call("update_task", json!({"id": "T-1", "state": "doing"})).await;
    assert!(ok && t["state"] == "doing", "{t}");
    let (ok, err) = call("update_task", json!({"id": "T-1", "state": "finished"})).await;
    assert!(!ok && err.as_str().unwrap().contains("todo, doing"), "{err}");

    let (ok, q) = call(
        "ask_question",
        json!({"title": "Tabs A or B?", "options": [{"title": "A"}, {"title": "B"}], "recommended": 1, "blocks": ["T-1"]}),
    )
    .await;
    assert!(ok, "{q}");
    assert_eq!(q["id"], "vinculum/Q-1");
    assert!(!call("ask_question", json!({"title": "x", "options": [{"title": "A"}], "recommended": 2})).await.0);

    let (ok, d) = call(
        "record_decision",
        json!({"title": "Tabs on the left", "owner_words": "A, the vertical list", "source": "owner", "answers": "Q-1"}),
    )
    .await;
    assert!(ok, "{d}");
    let (ok, d2) = call(
        "record_decision",
        json!({"title": "Tabs on top after all", "owner_words": "B", "source": "relayed by Viber Consultant", "supersedes": "D-1"}),
    )
    .await;
    assert!(ok, "{d2}");
    let (_, rec) = call("get_record", json!({"id": "D-1"})).await;
    assert_eq!(rec["state"], "replaced");
    assert!(rec["links"].as_array().unwrap().iter().any(|l| l["relation"] == "Replaced by"));

    let (_, found) = call("find_records", json!({"kind": "task", "milestone": "M4"})).await;
    assert_eq!(found["count"], 1, "steps count as their milestone");
    // A replaced decision is out of the default reads; include_closed brings it.
    let (_, found) = call("find_records", json!({"project": "all", "kind": "decision", "text": "vertical"})).await;
    assert_eq!(found["count"], 0, "{found}");
    let (_, found) = call("find_records", json!({"project": "all", "kind": "decision", "text": "vertical", "include_closed": true})).await;
    assert_eq!(found["records"][0]["id"], "vinculum/D-1");

    // Another project's agent can read but not change.
    let other_sess = mcp_session(&s, &other).await;
    let (ok, _) = mcp_tool(&s, &other, &other_sess, "get_record", json!({"id": "vinculum/T-1"})).await;
    assert!(ok);
    let (ok, err) =
        mcp_tool(&s, &other, &other_sess, "update_task", json!({"id": "vinculum/T-1", "state": "done"})).await;
    assert!(!ok && err.as_str().unwrap().contains("only your own project"), "{err}");
    let (ok, _) =
        mcp_tool(&s, &other, &other_sess, "add_milestone", json!({"project": "vinculum", "name": "M9", "title": "x"}))
            .await;
    assert!(!ok);
    // A device token only reads.
    let device = s.token(TokenKind::Device, None).await;
    let dev_sess = mcp_session(&s, &device).await;
    let (ok, _) = mcp_tool(&s, &device, &dev_sess, "project_info", json!({"project": "vinculum"})).await;
    assert!(ok);
    let (ok, _) =
        mcp_tool(&s, &device, &dev_sess, "update_project", json!({"project": "vinculum", "description": "x"})).await;
    assert!(!ok);
    // Refusals are logged as warnings.
    assert!(s.log_summaries().await.iter().any(|l| l.starts_with("update_task refused")));
}

#[tokio::test]
async fn the_owner_answers_on_the_page_and_the_agent_reads_it() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    mcp_tool(
        &s,
        &token,
        &sess,
        "ask_question",
        json!({"title": "A or B?", "options": [{"title": "A"}, {"title": "B"}]}),
    )
    .await;

    // Pages render for the owner, and a token can't answer.
    let cookie = s.owner_cookie().await;
    for path in [
        "/projects",
        "/p/vinculum",
        "/p/vinculum?tab=tasks",
        "/p/vinculum?tab=decisions",
        "/p/vinculum?tab=questions",
        "/p/vinculum/Q-1",
    ] {
        let res = s.http.get(s.url(path)).header(COOKIE, &cookie).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
    }
    assert_eq!(
        s.http.get(s.url("/p/nope")).header(COOKIE, &cookie).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.http.get(s.url("/p/vinculum/Q-9")).header(COOKIE, &cookie).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    let res =
        s.http.post(s.url("/p/vinculum/Q-1/answer")).bearer_auth(&token).form(&[("option", "1")]).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    let res = s
        .http
        .post(s.url("/p/vinculum/Q-1/answer"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("option", "2"), ("words", "B, smaller")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()[LOCATION], "/p/vinculum/Q-1?error=both", "an option or own words, not both");
    let page = s.http.get(s.url("/p/vinculum/Q-1?error=both")).header(COOKIE, &cookie).send().await.unwrap();
    assert!(page.text().await.unwrap().contains("not both"));
    let res = s
        .http
        .post(s.url("/p/vinculum/Q-1/answer"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("option", "2"), ("words", ""), ("next", "//evil.example")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[LOCATION], "/?answered=vinculum/Q-1", "no open redirects; back to the Dashboard");

    let (_, q) = mcp_tool(&s, &token, &sess, "get_record", json!({"id": "Q-1"})).await;
    assert_eq!((q["state"].as_str(), q["data"]["answer_option"].as_u64()), (Some("answered"), Some(2)));
    // Answering twice goes back to the question with the reason.
    let res = s
        .http
        .post(s.url("/p/vinculum/Q-1/answer"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("option", "1")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()[LOCATION], "/p/vinculum/Q-1?error=answered");
    let page = s.http.get(s.url("/p/vinculum/Q-1?error=answered")).header(COOKIE, &cookie).send().await.unwrap();
    assert!(page.text().await.unwrap().contains("This question was already answered."));
}

#[tokio::test]
async fn an_agent_waits_for_the_owners_answer() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    mcp_tool(
        &s,
        &token,
        &sess,
        "ask_question",
        json!({"title": "A or B?", "options": [{"title": "A"}, {"title": "B"}]}),
    )
    .await;

    // Nothing yet: the wait times out empty.
    let r: Value =
        s.http.get(s.url("/api/wait?timeout=1")).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["timed_out"], true);

    let waiter = {
        let (http, url, token) = (s.http.clone(), s.url("/api/wait?timeout=30&record=Q-1"), token.clone());
        tokio::spawn(
            async move { http.get(url).bearer_auth(token).send().await.unwrap().json::<Value>().await.unwrap() },
        )
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let cookie = s.owner_cookie().await;
    s.http
        .post(s.url("/p/vinculum/Q-1/answer"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("option", "2")])
        .send()
        .await
        .unwrap();
    let r = tokio::time::timeout(std::time::Duration::from_secs(10), waiter).await.unwrap().unwrap();
    let what = r["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["what"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(what.contains("Owner answered Q-1: 2: B"), "{what}");

    // Read now: since_last_check has nothing new, and the owner's token can't wait.
    let (_, r) = mcp_tool(&s, &token, &sess, "since_last_check", json!({})).await;
    assert_eq!(r["events"].as_array().unwrap_or_else(|| panic!("{r}")).len(), 0, "{r}");
    let res = s.http.get(s.url("/api/wait?timeout=1")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_dashboard_features_a_question_and_answers_on_send() {
    let s = TestServer::start().await;
    let cookie = s.owner_cookie().await;
    let html = s.http.get(s.url("/")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(html.contains("Nothing is waiting on you"));

    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    mcp_tool(&s, &token, &sess, "ask_question", json!({"title": "Old question"})).await;
    mcp_tool(
        &s,
        &token,
        &sess,
        "ask_question",
        json!({"title": "Tabs A or B?", "options": [{"title": "A"}, {"title": "B"}], "urgent": true}),
    )
    .await;
    let html = s.http.get(s.url("/")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    // Choosing an option sends nothing: they're radio buttons, none chosen beforehand, in the same
    // form as the own words and the one Send button (the owner, 2026-11-11).
    let words = html.find(r#"name="words""#).unwrap();
    let form_start = html[..words].rfind("<form").unwrap();
    let form_end = words + html[words..].find("</form>").unwrap();
    let form = &html[form_start..form_end];
    assert!(form.contains(r#"type="radio" name="option""#) && !form.contains("checked"), "{form}");
    assert!(!form.contains(r#"type="submit" name="option""#), "an option button would send at once");
    // The urgent one is featured, the other is listed, and the badge counts both.
    let featured = html.find("Tabs A or B?").unwrap();
    assert!(featured < html.find("Old question").unwrap());
    assert!(html.contains("More questions · 1"));
    assert!(html.contains(r#"<span class="mono count">2</span>"#));

    let res = s
        .http
        .post(s.url("/p/vinculum/Q-2/answer"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("option", "1"), ("next", "/?answered=vinculum/Q-2")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()[LOCATION], "/?answered=vinculum/Q-2");
    let html = s
        .http
        .get(s.url("/?answered=vinculum/Q-2"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("Sent to vinculum: vinculum/Q-2"), "notice");
    assert!(html.contains(r#"<span class="mono count">1</span>"#));
}

#[tokio::test]
async fn prs_merge_in_order_from_the_owners_buttons() {
    use crate::github::fake::pr;
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let call = |name: &'static str, args: Value| {
        let (s, token, sess) = (&s, token.clone(), sess.clone());
        async move { mcp_tool(s, &token, &sess, name, args).await }
    };
    assert!(!call("update_pr", json!({"number": 48, "ready": true})).await.0, "no repo yet");
    call("update_project", json!({"repo": "giosuel/vinculum"})).await;
    s.github.set("giosuel/vinculum", pr(48, "main", "b48"));
    s.github.set("giosuel/vinculum", pr(49, "b48", "b49"));
    crate::poller::poll_once(&s.state).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let wd = dir.path().display().to_string();
    for n in [48, 49] {
        let (ok, r) = call("update_pr", json!({"number": n, "ready": true})).await;
        assert!(ok, "{r}");
        let (ok, r) = call("add_button", json!({"record": format!("PR-{n}"), "label": "Merge", "kind": "merge", "command": format!("echo merged {n} > merged-{n}"), "workdir": wd})).await;
        assert!(ok, "{r}");
    }
    let (ok, r) =
        call("add_release", json!({"tag": "v0.0.5-m4", "title": "M4: Keybinds", "waits_for": ["PR-48", "PR-49"], "tag_button": false}))
            .await;
    assert!(ok, "{r}");
    call("add_button", json!({"record": "R-1", "label": "Push tag v0.0.5-m4", "kind": "tag", "command": "echo tagged > tagged", "workdir": wd, "requires": ["PR-49"]})).await;

    let (_, order) = call("pull_requests", json!({})).await;
    assert_eq!(order["merge_order"][0]["mergeable_now"], true);
    assert_eq!(order["merge_order"][1]["buttons"][0]["locked_because"], "merge #48 first");

    let cookie = s.owner_cookie().await;
    let html =
        s.http.get(s.url("/p/vinculum?tab=prs")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(html.contains("Merge #48–#49"), "stack button");
    let start = html.find(r#"name="actions" value=""#).unwrap() + r#"name="actions" value=""#.len();
    let ids = html[start..].split('"').next().unwrap().to_owned();

    // A token can't press, not even the consultant's.
    let admin = s.token(TokenKind::Admin, None).await;
    let res = s
        .http
        .post(s.url("/p/vinculum/merge-up-to/49"))
        .bearer_auth(&admin)
        .form(&[("actions", ids.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    s.github.merge_on_fetch.lock().unwrap().extend([48, 49]);
    let res = s
        .http
        .post(s.url("/p/vinculum/merge-up-to/49"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("actions", ids.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()[LOCATION], "/p/vinculum?tab=prs&merged=2&upto=49");
    assert!(dir.path().join("merged-48").exists() && dir.path().join("merged-49").exists());

    // The release unlocked; its tag button runs and marks it tagged; the agent hears about all of it.
    let html = s.http.get(s.url("/p/vinculum/R-1")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    let hstart = html.find(r#"name="hash" value=""#).unwrap() + r#"name="hash" value=""#.len();
    let hash = html[hstart..].split('"').next().unwrap().to_owned();
    let tag_id = {
        let i = html.find("/actions/").unwrap() + "/actions/".len();
        html[i..].split('/').next().unwrap().to_owned()
    };
    let res = s
        .http
        .post(s.url(&format!("/actions/{tag_id}/run")))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("hash", hash.as_str()), ("next", "/p/vinculum/R-1")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()[LOCATION], format!("/p/vinculum/R-1?ran={tag_id}&ok=1"));
    assert!(dir.path().join("tagged").exists());
    let (_, rel) = call("get_record", json!({"id": "R-1"})).await;
    assert_eq!(rel["state"], "tagged");
    let (_, news) = call("since_last_check", json!({})).await;
    let what = news.to_string();
    assert!(what.contains("#48 merged") && what.contains("Push tag v0.0.5-m4") && what.contains("done"), "{what}");
}

#[test]
fn return_addresses_stay_on_our_pages() {
    assert_eq!(local_path(Some("/p/v/Q-1")).as_deref(), Some("/p/v/Q-1"));
    assert_eq!(local_path(Some("/?answered=v/Q-1")).as_deref(), Some("/?answered=v/Q-1"));
    for bad in ["//evil.example", "/\\evil.example", "https://evil.example", "/x\r\nSet-Cookie: a=b", "/ä", "", "p/v"]
    {
        assert_eq!(local_path(Some(bad)), None, "{bad:?}");
    }
}

#[tokio::test]
async fn one_broken_pr_doesnt_stop_the_poll() {
    use crate::github::fake::pr;
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    mcp_tool(&s, &token, &sess, "update_project", json!({"repo": "giosuel/vinculum"})).await;
    s.github.set("giosuel/vinculum", pr(70, "main", "b70"));
    s.github.set("giosuel/vinculum", pr(71, "main", "b71"));
    crate::poller::poll_once(&s.state).await.unwrap();
    // #70 vanishes from GitHub entirely (the fake can't answer for it); #71 gets merged.
    s.github.prs.lock().unwrap().remove(&("giosuel/vinculum".to_owned(), 70));
    let mut merged = pr(71, "main", "b71");
    merged.state = "merged".into();
    s.github.set("giosuel/vinculum", merged);
    crate::poller::poll_once(&s.state).await.unwrap();
    let (_, r) = mcp_tool(&s, &token, &sess, "get_record", json!({"id": "PR-71"})).await;
    assert_eq!(r["state"], "merged", "the other PR was still synced");
    assert!(s.log_summaries().await.iter().any(|l| l.contains("giosuel/vinculum#70 failed")));
}

#[tokio::test]
async fn a_build_goes_from_agent_to_owner_and_feedback_comes_back() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("imaginarium")).await;
    let other = s.token(TokenKind::Project, Some("vinculum")).await;
    let device = s.token(TokenKind::Device, None).await;

    // A folder with an app and a data file, packed like `flow publish` does.
    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(src.path().join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(src.path().join("data.txt"), b"data").unwrap();
    let mut tar = Vec::new();
    crate::builds::pack(src.path(), &mut tar).unwrap();
    let features = serde_json::to_string(&["Tables: print a document with a table", "Pictures"]).unwrap();
    let url = |q: &str| {
        s.url(&format!("/api/builds?product=imaginarium&summary=Tables%20in%20print&commit=a1b2c3d&branch=m8&{q}"))
    };

    // Another project can't publish this product; a folder needs its entry.
    let res = s.http.post(url("entry=run.sh")).bearer_auth(&other).body(tar.clone()).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = s.http.post(url("")).bearer_auth(&token).body(tar.clone()).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    let res: Value = s
        .http
        .post(url(&format!(
            "entry=run.sh&features={}",
            features
                .replace(' ', "%20")
                .replace('"', "%22")
                .replace('[', "%5B")
                .replace(']', "%5D")
                .replace(',', "%2C")
                .replace(':', "%3A")
        )))
        .bearer_auth(&token)
        .body(tar.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["number"], 1, "{res}");
    assert_eq!(res["run"], "flow run imaginarium 1");

    // The owner's machine lists it and gets the same bytes back.
    let list: Value =
        s.http.get(s.url("/api/builds/imaginarium")).bearer_auth(&device).send().await.unwrap().json().await.unwrap();
    assert_eq!(list["builds"][0]["entry"], "run.sh");
    let back = s
        .http
        .get(s.url("/api/builds/imaginarium/1/archive"))
        .bearer_auth(&device)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(back.as_ref(), tar.as_slice());
    let detail: Value =
        s.http.get(s.url("/api/builds/imaginarium/1")).bearer_auth(&device).send().await.unwrap().json().await.unwrap();
    assert_eq!(detail["files"].as_array().map(Vec::len), Some(2), "{detail}");
    // A commit becomes part of the file name, so only hex is taken.
    let res = s
        .http
        .post(s.url("/api/builds?product=imaginarium&summary=x&commit=..%2F..%2Fx&entry=run.sh"))
        .bearer_auth(&token)
        .body(tar.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // What the agent wants tried is on the To test list, and on the build's page below Try it; no
    // feedback form any more.
    let cookie = s.owner_cookie().await;
    let page = s.http.get(s.url("/")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("imaginarium: 2 to test"), "the Dashboard's To test group");
    let page =
        s.http.get(s.url("/p/imaginarium/B-1")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(!page.contains("How did it go?") && page.contains("flow run imaginarium 1"));
    let try_it = page.find("aria-labelledby=\"try-h\"").unwrap();
    let list = page.find("To test in this build").unwrap();
    assert!(try_it < list, "Try it comes first");
    assert!(page.contains("print a document with a table") && page.contains(">Pictures<"));

    // The owner ticks one and sends; the agent hears it, with the note.
    let (tables, pictures) =
        s.db.call(|c| {
            let id = |n: i64| -> anyhow::Result<i64> {
                Ok(crate::records::try_find(
                    c,
                    &crate::records::RecordRef::parse(&format!("imaginarium/TT-{n}"))?,
                    None,
                )?
                .unwrap()
                .id)
            };
            Ok((id(1)?, id(2)?))
        })
        .await
        .unwrap();
    let res = s
        .http
        .post(s.url("/p/imaginarium/to-test"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[
            (format!("t{tables}"), "1".to_owned()),
            ("note".into(), "crisp".into()),
            ("back".into(), "/p/imaginarium/B-1".into()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()[LOCATION], "/p/imaginarium/B-1?tested=1");
    let sess = mcp_session(&s, &token).await;
    let (_, news) = mcp_tool(&s, &token, &sess, "since_last_check", json!({})).await;
    assert!(news.to_string().contains("Owner tested, works: TT-1 Tables. Note: \\\"crisp\\\""), "{news}");

    // The owner told the agent about a problem: it stays open, with the problem.
    let (ok, r) = mcp_tool(
        &s,
        &token,
        &sess,
        "resolve_tests",
        json!({"ids": ["TT-2"], "result": "problem", "note": "the border is cut off"}),
    )
    .await;
    assert!(ok, "{r}");
    let (ok, _) = mcp_tool(
        &s,
        &token,
        &sess,
        "add_to_test",
        json!({"title": "Undo a table edit", "how": "Ctrl+Z", "category": "Tables"}),
    )
    .await;
    assert!(ok);
    // A new release takes what's open along.
    let (ok, r) = mcp_tool(&s, &token, &sess, "add_release", json!({"tag": "v0.9.0", "title": "M9"})).await;
    assert!(ok, "{r}");
    assert_eq!(r["to_test"], 2);
    let page = s
        .http
        .get(s.url("/p/imaginarium?tab=to-test"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("the border is cut off") && page.contains("new in v0.9.0") && !page.contains(">Tables</label>"),
        "{page}"
    );
    let _ = pictures;

    // A Windows build: not for flow run, a download on its page, with what it needs.
    let exe = tempfile::tempdir().unwrap();
    std::fs::write(exe.path().join("lumen.exe"), b"MZ fake exe").unwrap();
    let mut wtar = Vec::new();
    crate::builds::pack(&exe.path().join("lumen.exe"), &mut wtar).unwrap();
    let wurl = |q: &str| s.url(&format!("/api/builds?product=imaginarium&summary=Windows&commit=b2c3d4e&branch=m8&{q}"));
    let res = s.http.post(wurl("entry=lumen.exe&platform=windows%20x86")).bearer_auth(&token).body(wtar.clone()).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST, "a platform is os-arch");
    let res: Value = s
        .http
        .post(wurl("entry=lumen.exe&platform=windows-x86_64&needs=%5B%22Windows%2010%22%2C%22Vulkan%22%5D"))
        .bearer_auth(&token)
        .body(wtar)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(res["run"].as_str().is_some_and(|r| r.starts_with("download it from")), "{res}");
    let n = res["number"].as_i64().unwrap();
    let list: Value = s.http.get(s.url("/api/builds/imaginarium")).bearer_auth(&device).send().await.unwrap().json().await.unwrap();
    let w = list["builds"].as_array().unwrap().iter().find(|b| b["number"] == n).cloned().unwrap();
    assert_eq!(w["platform"], "windows-x86_64");
    let cookie = s.owner_cookie().await;
    let page = s.http.get(s.url(&format!("/p/imaginarium/B-{n}"))).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Windows · x86_64") && page.contains("/download") && page.contains("Vulkan") && !page.contains(&format!("flow run imaginarium {n}")), "{page}");
    let file = s.http.get(s.url(&format!("/p/imaginarium/B-{n}/download"))).header(COOKIE, &cookie).send().await.unwrap();
    assert!(file.headers()["content-disposition"].to_str().unwrap().contains("lumen.exe"));
    assert_eq!(file.bytes().await.unwrap().as_ref(), b"MZ fake exe");
}

#[tokio::test]
async fn older_builds_move_over_with_their_number_and_date() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("imaginarium")).await;
    let device = s.token(TokenKind::Device, None).await;
    let src = tempfile::tempdir().unwrap();
    let bin = src.path().join("imaginarium");
    std::fs::write(&bin, b"#!/bin/sh\necho hi\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut tar = Vec::new();
    crate::builds::pack(&bin, &mut tar).unwrap();
    let post = |q: String| s.http.post(s.url(&format!("/api/builds?product=imaginarium&summary=Old%20one&{q}"))).bearer_auth(&token).body(tar.clone()).send();

    // viber-run's #63, then its #5 from October: each keeps its number, date and name.
    let res: Value = post("commit=79916898&branch=feat%2Fstroke-dash-gap&built_at=2026-11-11%2021%3A37&number=63".into()).await.unwrap().json().await.unwrap();
    assert_eq!(res["number"], 63, "{res}");
    assert_eq!(res["name"], "2026-11-11-2137-stroke-dash-gap-79916898");
    let res: Value = post("commit=2925797&branch=feat%2Fm6.12-text-style&built_at=2026-10-26%2010%3A28&number=5".into()).await.unwrap().json().await.unwrap();
    assert_eq!(res["number"], 5, "{res}");
    assert_eq!(res["name"], "2026-10-26-1028-m6.12-text-style-2925797");

    // A taken number, a number without a date, a date ahead, and something to try are refused.
    let res = post("commit=2925797&built_at=2026-10-26%2010%3A28&number=5".into()).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    for q in ["commit=2925797&number=6", "commit=2925797&built_at=2099-01-01%2000%3A00&number=6", "built_at=2026-10-26%2010%3A28&number=6&features=%5B%22A%3A%20b%22%5D", "built_at=yesterday&number=6"] {
        assert_eq!(post(q.into()).await.unwrap().status(), StatusCode::BAD_REQUEST, "{q}");
    }

    // The newest is still the newest, the October one is dated October, and the next new build
    // continues after the highest number.
    let list: Value = s.http.get(s.url("/api/builds/imaginarium")).bearer_auth(&device).send().await.unwrap().json().await.unwrap();
    let numbers: Vec<i64> = list["builds"].as_array().unwrap().iter().map(|b| b["number"].as_i64().unwrap()).collect();
    assert_eq!(numbers, [63, 5]);
    let old = s.state.db.call(|c| crate::records::find_str(c, "imaginarium/B-5", None)).await.unwrap();
    assert_eq!(old.created_at, 1_793_010_480_000, "2026-10-26 10:28 UTC");
    let res: Value = post("commit=a1b2c3d&branch=main".into()).await.unwrap().json().await.unwrap();
    assert_eq!(res["number"], 64, "{res}");

    // A long summary (viber-run's #7 had 292 characters) goes through; the client asks first,
    // and a refusal then comes back as an answer rather than as a dropped upload.
    let long = "x".repeat(292);
    let ask = |summary: String| {
        s.http.post(s.url(&format!("/api/builds?product=imaginarium&summary={summary}&built_at=2026-10-27%2010%3A00&number=7&check=true"))).bearer_auth(&token).send()
    };
    let ok: Value = ask(long.clone()).await.unwrap().json().await.unwrap();
    assert_eq!(ok["ok"], true);
    assert_eq!(ask("y".repeat(301)).await.unwrap().status(), StatusCode::BAD_REQUEST);
    let res = s.http.post(s.url("/api/builds?product=imaginarium&summary=x&built_at=2026-10-27%2010%3A00&number=5&check=true")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT, "a taken number is known before the upload");
    let res: Value = s.http.post(s.url(&format!("/api/builds?product=imaginarium&summary={long}&commit=abcdef1&built_at=2026-10-27%2010%3A00&number=7"))).bearer_auth(&token).body(tar.clone()).send().await.unwrap().json().await.unwrap();
    assert_eq!(res["number"], 7, "{res}");

    // Nothing was put on the To test list for the old ones.
    let tests: i64 = s.state.db.call(|c| Ok(c.query_row("SELECT count(*) FROM records WHERE kind = 'test'", [], |r| r.get(0))?)).await.unwrap();
    assert_eq!(tests, 0);
}

#[tokio::test]
async fn contributions_are_watched_for_their_project() {
    use crate::github::ItemInfo;
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("imaginarium")).await;
    let consultant = s.token(TokenKind::Admin, None).await;
    let item = |state: &str, comments| ItemInfo {
        title: "Undo leaves content behind".into(),
        url: "https://github.com/loro-dev/loro/issues/1255".into(),
        is_pr: false,
        state: state.into(),
        comments,
        ..Default::default()
    };
    s.github.items.lock().unwrap().insert(("loro-dev/loro".into(), 1255), item("open", 0));
    let cs = mcp_session(&s, &consultant).await;
    let (ok, r) = mcp_tool(
        &s,
        &consultant,
        &cs,
        "track_contribution",
        json!({"project": "imaginarium", "repo": "loro-dev/loro", "number": 1255}),
    )
    .await;
    assert!(ok, "{r}");
    assert_eq!(r["id"], "imaginarium/C-1");
    let (ok, _) = mcp_tool(
        &s,
        &consultant,
        &cs,
        "track_contribution",
        json!({"project": "imaginarium", "repo": "bad repo", "number": 1}),
    )
    .await;
    assert!(!ok);

    s.github.items.lock().unwrap().insert(("loro-dev/loro".into(), 1255), item("open", 1));
    crate::poller::poll_contributions(&s.state).await.unwrap();
    let ts = mcp_session(&s, &token).await;
    let (_, news) = mcp_tool(&s, &token, &ts, "since_last_check", json!({})).await;
    assert!(news.to_string().contains("1 new comment"), "{news}");
}

/// The owner, 2026-11-12: conversations log what sessions send each other with Claude Code's
/// SendMessage and what subagents are told and report, read from the transcripts.
#[tokio::test]
async fn conversations_log_sessions_and_subagents_from_the_transcripts() {
    let s = TestServer::start().await;
    let root = tempfile::tempdir().unwrap();
    let mine = root.path().join("-home-viber-agent-flow");
    let theirs = root.path().join("-home-viber-office");
    std::fs::create_dir_all(mine.join("s1/subagents")).unwrap();
    std::fs::create_dir_all(&theirs).unwrap();
    let line = |v: Value| format!("{v}\n");
    // Sent here, received there: logged once.
    std::fs::write(
        mine.join("s1.jsonl"),
        [
            line(json!({"type": "user", "cwd": "/home/viber/agent-flow", "timestamp": "2026-11-12T10:00:00Z", "message": {"content": "hi"}})),
            line(json!({"type": "assistant", "timestamp": "2026-11-12T10:01:00Z", "message": {"content": [
                {"type": "tool_use", "name": "SendMessage", "input": {"to": "Imaginarium", "message": "Scopes are live."}}]}})),
        ]
        .concat(),
    )
    .unwrap();
    std::fs::write(
        theirs.join("s2.jsonl"),
        [
            line(json!({"type": "custom-title", "customTitle": "Imaginarium", "sessionId": "s2"})),
            line(json!({"type": "queue-operation", "operation": "enqueue", "cwd": "/home/viber/office", "timestamp": "2026-11-12T10:01:01Z",
                "content": "<cross-session-message from=\"uds:/run/a.sock\" from-name=\"agent-flow-22\" from-mode=\"prompting\">\nScopes are live.\n</cross-session-message>"})),
            line(json!({"type": "assistant", "timestamp": "2026-11-12T10:05:00Z", "message": {"content": [
                {"type": "tool_use", "name": "SendMessage", "input": {"to": "uds:/run/a.sock", "message": "Done, **thanks**."}}]}})),
        ]
        .concat(),
    )
    .unwrap();
    // A subagent: its brief and its report; its tool calls stay out.
    std::fs::write(
        mine.join("s1/subagents/agent-a0ca2a7e879b8b08a.meta.json"),
        json!({"agentType": "general-purpose", "description": "Review 1: security"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        mine.join("s1/subagents/agent-a0ca2a7e879b8b08a.jsonl"),
        [
            line(json!({"type": "user", "parentUuid": null, "cwd": "/home/viber/agent-flow", "timestamp": "2026-11-12T11:00:00Z", "message": {"content": "Review the diff for security."}})),
            line(json!({"type": "user", "timestamp": "2026-11-12T11:00:05Z", "message": {"content": [{"type": "tool_result", "content": "secret file contents"}]}})),
            line(json!({"type": "assistant", "timestamp": "2026-11-12T11:20:00Z", "message": {"content": [
                {"type": "tool_use", "name": "SubagentHandback", "input": {"message": "No findings worth fixing."}}]}})),
        ]
        .concat(),
    )
    .unwrap();
    let path = root.path().to_owned();
    s.db.call(move |c| {
        crate::usage::ingest(c, &path)?;
        // Read again: nothing twice.
        crate::usage::ingest(c, &path)?;
        Ok(())
    })
    .await
    .unwrap();
    let cookie = s.owner_cookie().await;
    let page = s.http.get(s.url("/conversations")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Between sessions") && page.contains("Subagents") && page.contains("Review 1: security"), "{page}");
    let pair = s.http.get(s.url("/conversations?t=pair%3Aagent-flow%7Cimaginarium")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert_eq!(pair.matches("Scopes are live.").count(), 1, "kept once: {pair}");
    assert!(pair.contains("<strong>thanks</strong>") && pair.contains("→ agent-flow"), "{pair}");
    let sub = s.http.get(s.url("/conversations?t=sub%3Aa0ca2a7e879b8b08a")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(sub.contains("Review the diff for security.") && sub.contains("No findings worth fixing."), "{sub}");
    assert!(!sub.contains("secret file contents"), "tool results stay out");
    assert!(!page.contains("<textarea"), "no reply box");
    // Reload wakes the reader; tokens can't use the page or the button.
    let res = s.http.post(s.url("/transcripts/read")).header(COOKIE, &cookie).header(ORIGIN, &s.base).form(&[("next", "/conversations")]).send().await.unwrap();
    assert!(res.status().is_redirection(), "{}", res.status());
    let v = s.token(TokenKind::Project, Some("vinculum")).await;
    assert_eq!(s.http.get(s.url("/conversations")).bearer_auth(&v).send().await.unwrap().status(), StatusCode::FORBIDDEN);
    assert!(!s.http.post(s.url("/transcripts/read")).bearer_auth(&v).send().await.unwrap().status().is_success());
}

#[tokio::test]
async fn the_owner_edits_a_guide_and_agents_read_it() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let (_, list) = mcp_tool(&s, &token, &sess, "guides", json!({})).await;
    assert_eq!(list["guides"][0]["sections"], json!(["Introduction", "Ports"]));
    let (ok, err) = mcp_tool(&s, &token, &sess, "guide", json!({"name": "MACHINE.md", "section": "Nope"})).await;
    assert!(!ok && err.as_str().unwrap().contains("it has: Introduction, Ports"));
    let (ok, _) = mcp_tool(&s, &token, &sess, "guide", json!({"name": "../secret.md"})).await;
    assert!(!ok);

    let cookie = s.owner_cookie().await;
    let page = s
        .http
        .get(s.url("/guides?f=MACHINE.md&edit=1"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let start = page.find(r#"name="version" value=""#).unwrap() + r#"name="version" value=""#.len();
    let version = page[start..].split('"').next().unwrap().to_owned();
    let res = s
        .http
        .post(s.url("/guides/save"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[
            ("f", "MACHINE.md"),
            ("version", version.as_str()),
            ("section", "1"),
            ("text", "## Ports\n\nUse 10.0.8.1 or 127.0.0.1."),
        ])
        .send()
        .await
        .unwrap();
    assert!(
        res.headers()[LOCATION].to_str().unwrap().starts_with("/guides?f=MACHINE.md&saved="),
        "{:?}",
        res.headers()[LOCATION]
    );
    let (_, ports) = mcp_tool(&s, &token, &sess, "guide", json!({"name": "MACHINE.md", "section": "ports"})).await;
    assert!(ports["text"].as_str().unwrap().contains("127.0.0.1"));
    // The same old version again: refused, nothing written.
    let res = s
        .http
        .post(s.url("/guides/save"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("f", "MACHINE.md"), ("version", version.as_str()), ("section", "1"), ("text", "## Ports\n\nx")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()[LOCATION], "/guides?f=MACHINE.md&error=stale");

    // A PR of the guides: shown, counted, merged by the owner.
    s.github.repo_open.lock().unwrap().push(crate::github::RepoPr {
        number: 27,
        title: "MACHINE: gnome-keyring is installed now".into(),
        url: "https://github.com/giosuel/ai-handbook/pull/27".into(),
        additions: 1,
        deletions: 1,
        files: vec!["MACHINE.md".into()],
        author: "giosuel".into(),
        created_at: String::new(),
        head_ref_oid: "a".repeat(40),
    });
    crate::poller::refresh_guide_prs(&s.state).await.unwrap();
    let html = s.http.get(s.url("/")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(html.contains("Guides to review · 1"));
    let res = s
        .http
        .post(s.url("/guides/prs/merge"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("number", "27"), ("head", &"b".repeat(40))])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST, "pushed to since the owner looked");
    assert!(s.github.merged.lock().unwrap().is_empty());
    let res = s
        .http
        .post(s.url("/guides/prs/merge"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("number", "27"), ("head", &"a".repeat(40))])
        .send()
        .await
        .unwrap();
    // The test's checkout has no remote, so the pull after the merge reports a problem.
    assert_eq!(res.headers()[LOCATION], "/guides?view=prs&error=pull");
    assert_eq!(*s.github.merged.lock().unwrap(), vec![27]);
    let res = s
        .http
        .post(s.url("/guides/prs/merge"))
        .bearer_auth(&token)
        .form(&[("number", "27"), ("head", "")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_log_page_filters_and_streams() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    s.http.get(s.url("/api/whoami")).bearer_auth(&token).send().await.unwrap();
    let cookie = s.owner_cookie().await;
    let html = s
        .http
        .get(s.url("/log?q=actor:vinculum&range=1h"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("GET /api/whoami"), "the agent's request is listed");
    assert!(!html.contains("owner signed in with a one-time link"), "filtered to the agent");
    let html =
        s.http.get(s.url("/log?q=-actor:vinculum")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(!html.contains("GET /api/whoami"));
    // Owner only, page and stream.
    assert_eq!(s.http.get(s.url("/log")).bearer_auth(&token).send().await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(
        s.http.get(s.url("/log/stream?after=0")).bearer_auth(&token).send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let res = s.http.get(s.url("/log/stream?after=0")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(res.headers()["content-type"], "text/event-stream");
}

#[tokio::test]
async fn usage_and_machine_pages_show_the_counts() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("imaginarium")).await;
    let sess = mcp_session(&s, &token).await;
    mcp_tool(&s, &token, &sess, "update_project", json!({"checkout": "/home/viber/office"})).await;
    let dir = std::path::PathBuf::from(&s.state.config.transcripts_dir).join("-home-viber-office");
    std::fs::create_dir_all(&dir).unwrap();
    let now = time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap();
    std::fs::write(
        dir.join("s1.jsonl"),
        format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{now}\",\"sessionId\":\"s1\",\"message\":{{\"id\":\"m1\",\"model\":\"claude-opus-5-5\",\"usage\":{{\"input_tokens\":1200000,\"output_tokens\":5,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":9}}}}}}\n"
        ),
    )
    .unwrap();
    let stats = super::usage_round(&s.state).await.unwrap();
    assert_eq!(stats.calls, 1);
    let cookie = s.owner_cookie().await;
    let get = |path: &'static str| {
        let (http, url, cookie) = (s.http.clone(), s.url(path), cookie.clone());
        async move { http.get(url).header(COOKIE, cookie).send().await.unwrap() }
    };
    let html = get("/usage").await.text().await.unwrap();
    assert!(html.contains("imaginarium") && html.contains("1.2 M"), "{html}");
    assert!(get("/usage?tab=tasks").await.text().await.unwrap().contains("Not in a task"));
    // Days are the browser's: today's call is on today's local date at UTC+14 and at UTC−11 alike.
    for zone in ["Pacific/Kiritimati", "Pacific/Pago_Pago"] {
        let day = crate::tz::Tz::named(zone).unwrap().date(crate::util::now_ms());
        let res = s
            .http
            .get(s.url(&format!("/usage?day={day}")))
            .header(COOKIE, format!("{cookie}; af_tz={zone}"))
            .send()
            .await
            .unwrap();
        let html = res.text().await.unwrap();
        assert!(html.contains(&format!("Hours in {zone}")) && html.contains("1.2 M"), "{zone}: {html}");
    }
    // The phone's More tab lists the sections that don't fit in the tab bar.
    let more = get("/more").await.text().await.unwrap();
    assert!(more.contains("Conversations") && more.contains("class=\"tabbar\""), "{more}");
    let res = get("/machine?range=6h").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.text().await.unwrap().contains("GB free of"));
    // The live figures: one per tile, in the same order.
    let now: serde_json::Value = get("/machine/now").await.json().await.unwrap();
    assert_eq!(now["tiles"].as_array().unwrap().len(), 5);
    assert!(now["tiles"][0]["small"].as_str().unwrap().contains(" cores"));
    let res = s.http.get(s.url("/machine/now")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let html = get("/p/imaginarium").await.text().await.unwrap();
    assert!(html.contains("Tokens this week") && html.contains("1.2 M"));
    // Agents don't see it.
    let res = s.http.get(s.url("/usage")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_token_in_two_places_is_reported() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    // The test server's peer is a trusted proxy, so it may say who the client is.
    for from in ["10.0.8.1", "203.0.113.7", "10.0.8.1", "203.0.113.7"] {
        let res =
            s.http.get(s.url("/api/whoami")).bearer_auth(&token).header("x-forwarded-for", from).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
    let log = s.log_summaries().await;
    assert!(log.iter().any(|l| l.contains("used from 10.0.8.1 and 203.0.113.7")), "{log:?}");
    assert_eq!(
        log.iter().filter(|l| l.contains("within ten minutes")).count(),
        1,
        "once a day, however often it alternates"
    );
}

#[tokio::test]
async fn only_the_owner_starts_a_project_and_slugs_stay_unique() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Admin, None).await;
    let res = s.http.get(s.url("/projects/new")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    s.token(TokenKind::Project, Some("vinculum")).await;
    let cookie = s.owner_cookie().await;
    // A taken slug is refused before anything is created; the form keeps what was typed.
    let html = s
        .http
        .post(s.url("/projects/new"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("name", "Vinculum"), ("folder", "~/x-agentflow-test-never-created"), ("description", "kept")])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("exists already") && html.contains("kept"), "{html}");
    assert!(!crate::util::home_dir().join("x-agentflow-test-never-created").exists());
    // And from another site, not at all.
    let res = s
        .http
        .post(s.url("/projects/new"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, "https://evil.example")
        .form(&[("name", "Evil")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn steps_run_on_batches_blocks_and_conclusions() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let call = |name: &'static str, args: Value| {
        let (s, token, sess) = (&s, token.clone(), sess.clone());
        async move { mcp_tool(s, &token, &sess, name, args).await }
    };
    assert!(call("add_milestone", json!({"name": "M4", "title": "Keybinds"})).await.0);
    assert!(call("add_milestone", json!({"name": "K1", "title": "Read binds", "step_of": "M4"})).await.0);
    assert!(call("add_milestone", json!({"name": "K2", "title": "Write binds", "step_of": "M4"})).await.0);
    // All or nothing, every problem listed.
    let (ok, err) = call("add_tasks", json!({"milestone": "K1", "tasks": [
        {"title": "Parse", "estimate": "2 h"}, {"title": "Store"}, {"title": "Skip me", "state": "skipped"}]})).await;
    assert!(!ok, "{err}");
    let err = err.as_str().unwrap();
    assert!(err.contains("#2: needs an estimate") && err.contains("#3: needs a reason"), "{err}");
    let (ok, r) = call("add_tasks", json!({"milestone": "K1", "tasks": [
        {"title": "Parse", "estimate": "2 h", "branch": "feat/binds"},
        {"title": "Store", "estimate": "40 min"},
        {"title": "Rename the module", "state": "skipped", "reason": "churn", "importance": "low"}]})).await;
    assert!(ok, "{r}");
    assert_eq!(r["ids"], json!(["T-1", "T-2", "T-3"]));
    // Order, then doing starts the step; one call moves two.
    let (ok, o) = call("order_tasks", json!({"milestone": "K1", "ids": ["T-2"]})).await;
    assert!(ok && o["order"] == json!(["T-2", "T-1", "T-3"]), "{o}");
    let (ok, u) = call("update_task", json!({"ids": ["T-1", "T-2"], "state": "doing"})).await;
    assert!(ok, "{u}");
    assert_eq!(u["started"], json!(["K1", "M4"]));
    // "Working on" comes from the tasks in doing until the agent says more.
    let ov = s.http.get(s.url("/p/vinculum")).header(COOKIE, s.owner_cookie().await).send().await.unwrap().text().await.unwrap();
    assert!(ov.contains("doing T-2 Store (+1)"), "{ov}");
    // Added once the step is under way: found, unless said.
    let (ok, t) = call("add_task", json!({"milestone": "K1", "title": "Wider drop zone", "estimate": "30 min", "source": "owner_feedback", "source_ref": "B-3"})).await;
    assert!(ok, "{t}");
    // A question blocks T-4; hello says so; the answer unblocks it.
    assert!(!call("update_task", json!({"id": "T-4", "state": "blocked"})).await.0);
    let (ok, q) = call("ask_question", json!({"title": "Wide or narrow?", "blocks": ["T-4"]})).await;
    assert!(ok, "{q}");
    let (_, hello) = call("hello", json!({"session": "V", "machine": "m", "model": "o", "effort": "h"})).await;
    let k1 = hello["where"].as_array().unwrap().iter().find(|w| w["step"] == "K1").cloned().unwrap();
    assert_eq!(k1["blocked"][0]["by"], "Q-1");
    assert_eq!(k1["doing"].as_array().unwrap().len(), 2);
    let tasks = s.http.get(s.url("/p/vinculum?tab=tasks")).header(COOKIE, s.owner_cookie().await).send().await.unwrap().text().await.unwrap();
    assert!(tasks.contains("blocked by Q-1") && tasks.contains("added · your feedback · from B-3"), "{tasks}");
    assert!(tasks.contains("skipped: churn"));
    // Concluding needs the open tasks dealt with.
    let (ok, err) = call("conclude_step", json!({"step": "K1", "summary": "Binds are read."})).await;
    assert!(!ok && err.as_str().unwrap().contains("still has open tasks"), "{err}");
    let (ok, c) = call("conclude_step", json!({"step": "K1", "summary": "Binds are read.", "move_open_to": "K2"})).await;
    assert!(ok, "{c}");
    assert_eq!(c["moved"].as_array().unwrap().len(), 3);
    assert!(!call("add_task", json!({"milestone": "K1", "title": "Late", "estimate": "1 h"})).await.0);
    // History brought in: done tasks may go into a concluded step, without estimates; the step
    // and milestone keep their real dates and summary.
    let (ok, r) = call("add_tasks", json!({"milestone": "K1", "tasks": [{"title": "Old work", "state": "done"}]})).await;
    assert!(ok, "{r}");
    let (ok, r) = call("update_milestone", json!({"milestone": "K1", "started_at": "2026-10-14", "closed_at": "2026-10-20 18:00", "summary": "Imported."})).await;
    assert!(ok, "{r}");
    assert!(!call("update_milestone", json!({"milestone": "K2", "closed_at": "2026-10-20"})).await.0, "only a closed one");
    assert!(!call("update_milestone", json!({"milestone": "K1", "started_at": "2099-01-01"})).await.0, "not in the future");
    // A parked task may have no milestone; nothing else may.
    let (ok, r) = call("add_task", json!({"title": "Windows parity", "state": "parked", "estimate": "2 d"})).await;
    assert!(ok, "{r}");
    assert!(!call("add_task", json!({"title": "Loose", "estimate": "1 h"})).await.0);
    let tasks = s.http.get(s.url("/p/vinculum?tab=tasks")).header(COOKIE, s.owner_cookie().await).send().await.unwrap().text().await.unwrap();
    assert!(tasks.contains("Without a milestone") && tasks.contains("Windows parity"), "{tasks}");
    // A review is a step: reviewers and commit, findings ranked, one skipped.
    assert!(!call("add_milestone", json!({"name": "R1", "title": "Review", "review": {"reviewers": ["UX"], "commit": "abc1234"}})).await.0);
    let (ok, r) = call("add_milestone", json!({"name": "K9", "title": "End-of-M4 review", "step_of": "M4",
        "review": {"reviewers": ["correctness", "UX"], "commit": "abc1234", "owner_words": "rate the issues by importance"}})).await;
    assert!(ok, "{r}");
    let (ok, r) = call("add_tasks", json!({"milestone": "K9", "tasks": [
        {"title": "Low thing", "estimate": "20 min", "source": "review", "importance": "low"},
        {"title": "Big thing", "estimate": "2 h", "source": "review", "importance": "high"},
        {"title": "Churn", "state": "skipped", "reason": "no behaviour change", "importance": "low", "source": "review"}]})).await;
    assert!(ok, "{r}");
    let tasks = s.http.get(s.url("/p/vinculum?tab=tasks")).header(COOKIE, s.owner_cookie().await).send().await.unwrap().text().await.unwrap();
    let at = tasks.find("End-of-M4 review").unwrap();
    assert!(tasks[at..].contains("review step") && tasks[at..].contains("2 reviewers · commit abc1234"), "{tasks}");
    let step_url = tasks[at..].split("href=\"").nth(1).unwrap().split('"').next().unwrap().to_owned();
    let page = s.http.get(s.url(&step_url)).header(COOKIE, s.owner_cookie().await).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Findings, by importance") && page.contains("rate the issues by importance"), "{page}");
    assert!(page.find("Big thing").unwrap() < page.find("Low thing").unwrap(), "high first");
    assert!(page.contains("skipped: no behaviour change"));
}

#[tokio::test]
async fn the_project_document_is_the_agents_and_the_owners() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let call = |name: &'static str, args: Value| {
        let (s, token, sess) = (&s, token.clone(), sess.clone());
        async move { mcp_tool(s, &token, &sess, name, args).await }
    };
    let (ok, r) = call("edit_project_doc", json!({"section": "How to work on it", "text": "- Conventional Commits\n- DCO sign-off"})).await;
    assert!(ok && r["rev"] == 1, "{r}");
    let (ok, r) = call("edit_project_doc", json!({"section": "Roadmap", "text": "- M4 Keybinds (now)"})).await;
    assert!(ok && r["rev"] == 2, "{r}");
    let (ok, err) = call("edit_project_doc", json!({"text": "x".repeat(17 * 1024)})).await;
    assert!(!ok && err.as_str().unwrap().contains("KB"), "{err}");
    let (_, hello) = call("hello", json!({"session": "V", "machine": "m", "model": "o", "effort": "h"})).await;
    let text = hello["doc"]["text"].as_str().unwrap();
    assert!(text.contains("## How to work on it") && text.contains("## Roadmap"), "{text}");
    assert!(hello["project"].get("stack").is_none(), "the stack lives in the document now");

    // The owner sees it like a guide and edits a section; an edit on an old version is refused.
    let cookie = s.owner_cookie().await;
    let page = s.http.get(s.url("/p/vinculum?tab=notes")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Project document") && page.contains("Conventional Commits") && page.contains("rev 2"), "{page}");
    let version = page.split("name=\"version\" value=\"").nth(1).map(|x| x.split('"').next().unwrap().to_owned());
    assert!(version.is_none(), "no form until a pencil is pressed");
    let page = s.http.get(s.url("/p/vinculum?tab=notes&edit=1")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    let version = page.split("name=\"version\" value=\"").nth(1).unwrap().split('"').next().unwrap().to_owned();
    let post = |v: String, text: &'static str| {
        let (s, cookie) = (&s, cookie.clone());
        async move {
            s.http
                .post(s.url("/p/vinculum/doc"))
                .header(COOKIE, &cookie)
                .header(ORIGIN, &s.base)
                .form(&[("version", v.as_str()), ("section", "1"), ("text", text)])
                .send()
                .await
                .unwrap()
        }
    };
    let res = post("stale0".into(), "## Roadmap\n\n- nope").await;
    assert!(res.headers()[LOCATION].to_str().unwrap().contains("doc=stale"));
    let res = post(version, "## Roadmap\n\n- M4 Keybinds\n- M5 Themes").await;
    assert!(res.headers()[LOCATION].to_str().unwrap().contains("doc=saved"));
    let (_, info) = call("project_info", json!({})).await;
    assert!(info["doc"]["text"].as_str().unwrap().contains("M5 Themes"), "{info}");
    assert_eq!(info["doc"]["rev"], 3);
}

#[tokio::test]
async fn notes_are_files_or_text_and_come_back() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let call = |name: &'static str, args: Value| {
        let (s, token, sess) = (&s, token.clone(), sess.clone());
        async move { mcp_tool(s, &token, &sess, name, args).await }
    };
    // A file: any kind, kept and offered for download, never as a page.
    let up = |q: &'static str, body: &'static [u8]| {
        let (s, token) = (&s, token.clone());
        async move { s.http.post(s.url(&format!("/api/notes?{q}"))).bearer_auth(&token).body(body).send().await.unwrap() }
    };
    let res = up("name=Morning%20report&category=Morning%20Report&filename=report.md", b"# Done\n\n- T-4").await;
    assert!(res.status().is_success(), "{}", res.text().await.unwrap());
    let res = up("name=Page&category=x&filename=evil.html", b"<script>alert(1)</script>").await;
    assert!(res.status().is_success());
    assert!(!up("name=&category=x&filename=a.txt", b"x").await.status().is_success(), "a name");
    // A new version keeps the old file.
    assert!(up("replaces=N-1&filename=report.md&name=&category=", b"# Done\n\n- T-4\n- T-5").await.status().is_success());
    // A text note that comes back until resolved, one on a past date, one later.
    let (ok, n) = call("add_note", json!({"name": "Loro needs im", "category": "known issue", "text": "Check on every Loro update.", "resurface": "when Loro updates"})).await;
    assert!(ok, "{n}");
    assert!(call("add_note", json!({"name": "x-wing pin", "category": "known issue", "text": "pinned", "resurface": "2020-01-01"})).await.0);
    assert!(call("add_note", json!({"name": "Later", "category": "known issue", "text": "later", "resurface": "2099-01-01"})).await.0);
    let (_, hello) = call("hello", json!({"session": "V", "machine": "m", "model": "o", "effort": "h"})).await;
    let mut back: Vec<&str> = hello["notes_back"].as_array().unwrap().iter().map(|n| n["name"].as_str().unwrap()).collect();
    back.sort_unstable();
    assert_eq!(back, ["Loro needs im", "x-wing pin"], "{hello}");
    assert!(!call("update_note", json!({"id": "N-3", "state": "resolved"})).await.0, "a reason");
    assert!(call("update_note", json!({"id": "N-3", "state": "resolved", "note": "Loro 1.3 dropped im"})).await.0);
    assert!(!call("update_note", json!({"id": "N-1", "text": "x"})).await.0, "files get a new version instead");

    let cookie = s.owner_cookie().await;
    let tab = s.http.get(s.url("/p/vinculum?tab=notes")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(tab.contains("Morning report") && tab.contains("morning report") && tab.contains("1 resolved"), "{tab}");
    assert!(tab.contains("comes back when Loro updates") || tab.contains("2 versions"), "{tab}");
    let page = s.http.get(s.url("/p/vinculum/N-1")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("<li>T-5</li>") && page.contains("v1"), "the newest version rendered: {page}");
    let f = s.http.get(s.url("/p/vinculum/N-2/file")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(f.headers()[CONTENT_TYPE], "application/octet-stream");
    assert!(f.headers()["content-disposition"].to_str().unwrap().starts_with("attachment"));
    let old = s.http.get(s.url("/p/vinculum/N-1/file?v=1")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(old.contains("T-4") && !old.contains("T-5"));
}

#[tokio::test]
async fn answers_make_scoped_decisions_and_releases_bring_their_tag_button() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let call = |name: &'static str, args: Value| {
        let (s, token, sess) = (&s, token.clone(), sess.clone());
        async move { mcp_tool(s, &token, &sess, name, args).await }
    };
    assert!(call("add_milestone", json!({"name": "M4", "title": "Keybinds"})).await.0);
    assert!(call("add_task", json!({"title": "Sidebar", "milestone": "M4", "estimate": "1 h"})).await.0);
    // No active milestone: project-wide by default.
    let (ok, q) = call("ask_question", json!({"title": "Port 8990?"})).await;
    assert!(ok && q["scope"] == "project", "{q}");
    assert!(call("update_milestone", json!({"milestone": "M4", "state": "active"})).await.0);
    let (ok, q) = call("ask_question", json!({"title": "List or grid?", "options": [{"title": "A list"}, {"title": "A grid"}], "recommended": 1, "blocks": ["T-1"]})).await;
    assert!(ok && q["scope"] == "M4" && q["id"] == "vinculum/Q-2", "{q}");
    // Answered in chat: one call answers, unblocks and records the decision in the question's scope.
    let (ok, r) = call("answer_question", json!({"id": "Q-2", "owner_words": "lets go with 1", "option": 1})).await;
    assert!(ok, "{r}");
    assert!(r["state"] == "answered" && r["scope"] == "M4", "{r}");
    let (_, d) = call("get_record", json!({"id": r["decision"].as_str().unwrap()})).await;
    let d = d.to_string();
    assert!(d.contains("List or grid: A list") && d.contains("lets go with 1") && d.contains("owner, in chat"), "{d}");
    let (_, t) = call("get_record", json!({"id": "T-1"})).await;
    assert_eq!(t["state"], "todo", "{t}");
    // record_decision with answers closes an open question too, in its scope.
    let (ok, d) = call("record_decision", json!({"title": "8990 it is", "owner_words": "yes", "source": "owner", "answers": "Q-1"})).await;
    assert!(ok, "{d}");
    let (_, q1) = call("get_record", json!({"id": "Q-1"})).await;
    assert_eq!(q1["state"], "answered", "{q1}");
    // Answered on the page: the decision is recorded by itself, and the agent hears it with a
    // reminder to check older decisions.
    assert!(call("ask_question", json!({"title": "Tabs or a list?", "options": [{"title": "Tabs"}, {"title": "A list"}]})).await.0);
    let cookie = s.owner_cookie().await;
    let res = s.http.post(s.url("/p/vinculum/Q-3/answer")).header(COOKIE, &cookie).header(ORIGIN, &s.base).form(&[("option", "2")]).send().await.unwrap();
    assert!(res.status().is_redirection());
    let decisions = s.http.get(s.url("/p/vinculum?tab=decisions")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(decisions.contains("Tabs or a list: A list") && decisions.contains("M4 · "), "{decisions}");
    let (_, news) = call("since_last_check", json!({})).await;
    let news = news.to_string();
    assert!(news.contains("Owner answered Q-3: 2: A list → D-") && news.contains("check whether it changes an earlier decision"), "{news}");
    // Asked in a chat the owner is having: recorded, but no badge (and no push).
    assert!(!call("ask_question", json!({"title": "Now?", "in_chat": true, "urgent": true})).await.0);
    let before = s.db.call(|c| crate::waiting::count(c)).await.unwrap();
    let (ok, r) = call("ask_question", json!({"title": "Rename it to Atlas?", "in_chat": true})).await;
    assert!(ok, "{r}");
    assert_eq!(s.db.call(|c| crate::waiting::count(c)).await.unwrap(), before);
    let dash = s.http.get(s.url("/")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(dash.contains("Asked in chat") && dash.contains("Rename it to Atlas?"), "{dash}");

    // A cleanup the owner asked for: fix a decision, archive a duplicate, bring it back.
    let (ok, d) = call("record_decision", json!({"title": "Tabs on the left", "owner_words": "paraphrase, from memory: tabs left", "source": "owner", "tags": ["paraphrase"]})).await;
    assert!(ok, "{d}");
    let id = d["id"].as_str().unwrap().to_owned();
    assert!(!call("update_decision", json!({"id": id, "owner_words": "tabs on the left please"})).await.0, "a reason");
    let (ok, r) = call("update_decision", json!({"id": id, "owner_words": "tabs on the left please", "said_at": "2026-11-01", "tags": [], "reason": "owner asked: recover the exact words"})).await;
    assert!(ok && r["changed"] == true, "{r}");
    let (_, got) = call("get_record", json!({"id": id})).await;
    assert!(got.to_string().contains("tabs on the left please") && got.to_string().contains("owner asked: recover"), "{got}");
    let (ok, r) = call("update_decision", json!({"id": id, "state": "archived", "reason": "owner asked: duplicate of D-2"})).await;
    assert!(ok && r["state"] == "archived", "{r}");
    let decisions = s.http.get(s.url("/p/vinculum?tab=decisions")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(decisions.contains("1 archived"), "{decisions}");
    assert!(!call("update_decision", json!({"id": "D-1", "state": "deleted", "reason": "x"})).await.0);
    assert!(call("update_decision", json!({"id": id, "state": "current", "reason": "owner: keep it after all"})).await.0);

    // A release brings its Push tag button once the project has a repository: GitHub's API makes
    // the tag, so no folder is needed (the project may lie on any machine).
    let (ok, r) = call("add_release", json!({"tag": "v0.0.5-m4", "title": "M4: Keybinds"})).await;
    assert!(ok && r["tag_button"].as_str().unwrap_or("").starts_with("none"), "{r}");
    assert!(call("update_project", json!({"repo": "giosuel/vinculum"})).await.0);
    let (ok, r) = call("add_release", json!({"tag": "v0.0.6-m5", "title": "M5"})).await;
    assert!(ok, "{r}");
    let cmd = r["tag_button"]["command"].as_str().unwrap();
    assert!(cmd.contains("repos/giosuel/vinculum/git/ref/heads/main") && cmd.contains("-f tag=v0.0.6-m5") && cmd.contains("ref=refs/tags/v0.0.6-m5"), "{cmd}");
    assert!(!cmd.contains("git push") && !cmd.contains("checkout") && !cmd.contains("cd "), "{cmd}");
    // After a context reset: hello brief leaves out the document and milestones.
    let (_, b) = call("hello", json!({"session": "V", "machine": "m", "model": "o", "effort": "h", "brief": true})).await;
    assert!(b.get("doc").is_none() && b.get("milestones").is_none() && b.get("where").is_some(), "{b}");
    assert!(!call("add_release", json!({"tag": "v1; rm -rf ~", "title": "x"})).await.0);
    // Its text can be changed later; its tag can't, and other kinds aren't releases.
    let (ok, u) = call("update_release", json!({"id": r["id"], "body": "Closes M5. Build B-63."})).await;
    assert!(ok, "{u}");
    let (_, got) = call("get_record", json!({"id": r["id"]})).await;
    assert!(got.to_string().contains("Build B-63.") && got.to_string().contains("v0.0.6-m5"), "{got}");
    assert!(!call("update_release", json!({"id": r["id"]})).await.0);
    assert!(!call("update_release", json!({"id": r["id"], "title": " "})).await.0);
    assert!(!call("update_release", json!({"id": "D-1", "body": "x"})).await.0);
    // A question holds a release's tag button until it's answered.
    let rel = r["id"].as_str().unwrap().rsplit('/').next().unwrap().to_owned();
    let (ok, q) = call("ask_question", json!({"title": "Tag name: v0.0.6-m5 or v0.1.0?", "blocks": [rel.clone()]})).await;
    assert!(ok, "{q}");
    let page = s.http.get(s.url(&format!("/p/vinculum/{rel}"))).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("waits for the answer to Q-"), "{page}");

    // Batches: decisions (one replacing an earlier one of the same batch), ADRs, to-test items.
    let (ok, r) = call("record_decisions", json!({"decisions": [
        {"title": "Merge commits", "owner_words": "merge, never squash", "source": "owner", "said_at": "2026-10-20"},
        {"title": "Merge commits, branches deleted", "owner_words": "and delete the branches", "source": "owner", "supersedes": "#1"}]})).await;
    assert!(ok, "{r}");
    let first = r["ids"][0].as_str().unwrap().to_owned();
    let (_, old) = call("get_record", json!({"id": first})).await;
    assert_eq!(old["state"], "replaced", "{old}");
    let (ok, err) = call("record_decisions", json!({"decisions": [{"title": "x", "owner_words": "y", "source": "owner"}, {"title": "z", "owner_words": "", "source": "owner"}]})).await;
    assert!(!ok && err.as_str().unwrap().contains("#2"), "{err}");
    assert!(call("add_adrs", json!({"adrs": [{"title": "Rendering", "repo": "giosuel/vinculum", "path": "docs/adr/0001.md"}, {"title": "Storage", "repo": "giosuel/vinculum", "path": "docs/adr/0002.md"}]})).await.0);
    let (ok, r) = call("add_to_tests", json!({"items": [{"title": "Import", "category": "Import"}, {"title": "Export", "category": "Export"}]})).await;
    assert!(ok && r["ids"].as_array().unwrap().len() == 2, "{r}");

    // A note on a step; a step made a review later; where for a milestone whose steps are done.
    assert!(call("add_milestone", json!({"name": "R9", "title": "Review", "step_of": "M4"})).await.0);
    assert!(!call("update_milestone", json!({"milestone": "R9", "review": {"reports": ["N-1"]}})).await.0, "not a review yet");
    assert!(call("update_milestone", json!({"milestone": "R9", "review": {"reviewers": ["UX"], "commit": "abc1234"}})).await.0);
    // Only the reports: the reviewers and commit stay.
    assert!(call("update_milestone", json!({"milestone": "R9", "review": {"reports": ["N-26"]}})).await.0);
    let (_, m) = call("get_milestone", json!({"milestone": "R9"})).await;
    assert!(m["review"]["reports"] == json!(["N-26"]) && m["review"]["reviewers"] == json!(["UX"]) && m["review"]["commit"] == "abc1234", "{m}");
    let (ok, n) = call("add_note", json!({"name": "Review report R1", "category": "review report", "text": "Findings…", "record": "R9"})).await;
    assert!(ok, "{n}");
    assert!(!call("add_note", json!({"name": "x", "category": "x", "text": "x", "record": "NOPE"})).await.0);
    let tasks = s.http.get(s.url("/p/vinculum?tab=tasks")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    let at = tasks.find(">Review<").unwrap();
    let url = tasks[at..].split("href=\"").nth(1).unwrap().split('"').next().unwrap().to_owned();
    let step = s.http.get(s.url(&url)).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(step.contains("Reports and notes") && step.contains("Review report R1"), "{step}");
    assert!(call("conclude_step", json!({"step": "R9", "summary": "Reviewed."})).await.0);
    assert!(call("update_milestone", json!({"milestone": "M4", "state": "active"})).await.0);
    let (_, h) = call("hello", json!({"session": "V", "machine": "m", "model": "o", "effort": "h", "brief": true})).await;
    let w = h["where"].as_array().unwrap();
    assert!(w.iter().any(|x| x["milestone"] == "M4" && x["hint"].as_str().unwrap().contains("every step is concluded")), "{h}");
}

#[tokio::test]
async fn screenshots_are_uploaded_shown_and_linked() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    assert!(mcp_tool(&s, &token, &sess, "add_milestone", json!({"name": "M4", "title": "Keybinds"})).await.0);
    let (ok, t) = mcp_tool(&s, &token, &sess, "add_task", json!({"title": "Sidebar", "milestone": "M4", "estimate": "1 h"})).await;
    assert!(ok, "{t}");
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    png.extend_from_slice(&1280u32.to_be_bytes());
    png.extend_from_slice(&800u32.to_be_bytes());
    png.extend_from_slice(&[0; 64]);
    let up = |query: &'static str, body: Vec<u8>| {
        let (http, url, token) = (s.http.clone(), s.url(&format!("/api/screenshots?{query}")), token.clone());
        async move { http.post(url).bearer_auth(token).body(body).send().await.unwrap() }
    };
    let res =
        up("name=Sidebar%2C%20variant%20A&category=Mockup&description=Tabs%20on%20the%20left&record=T-1", png.clone())
            .await;
    assert_eq!(res.status(), StatusCode::OK);
    let j: Value = res.json().await.unwrap();
    assert_eq!(j["id"], "vinculum/S-1");
    assert!(up("name=x&category=bug", png.clone()).await.status().is_success());
    // Not an image (an SVG could run script from our origin), no category, a record of nothing.
    let svg = b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>".to_vec();
    assert_eq!(up("name=x&category=bug", svg).await.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(up("name=x&category=%20", png.clone()).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(up("name=x&category=bug&record=T-99", png.clone()).await.status(), StatusCode::BAD_REQUEST);

    let cookie = s.owner_cookie().await;
    let get = |path: String| {
        let (http, url, cookie) = (s.http.clone(), s.url(&path), cookie.clone());
        async move { http.get(url).header(COOKIE, cookie).send().await.unwrap() }
    };
    let tab = get("/p/vinculum?tab=screenshots".into()).await.text().await.unwrap();
    assert!(
        tab.contains("Sidebar, variant A") && tab.contains("/p/vinculum/S-1/image") && tab.contains("mockup"),
        "{tab}"
    );
    let only = get("/p/vinculum?tab=screenshots&cat=bug".into()).await.text().await.unwrap();
    assert!(!only.contains("Sidebar, variant A") && only.contains("/p/vinculum/S-2/image"), "{only}");
    let page = get("/p/vinculum/S-1".into()).await.text().await.unwrap();
    assert!(page.contains("1280 × 800") && page.contains("Tabs on the left"), "{page}");
    let task = get("/p/vinculum/T-1".into()).await.text().await.unwrap();
    assert!(task.contains("Screenshots") && task.contains("S-1"), "the task links to it: {task}");
    let img = get("/p/vinculum/S-1/image".into()).await;
    assert_eq!(img.headers()["content-type"], "image/png");
    assert_eq!(img.headers()["x-content-type-options"], "nosniff");
    assert_eq!(img.bytes().await.unwrap().as_ref(), png.as_slice());
    // Signed out: nothing.
    let res = s.http.get(s.url("/p/vinculum/S-1/image")).send().await.unwrap();
    assert!(res.status().is_client_error() || res.status().is_redirection(), "{}", res.status());

    // A mockup is proposed until approved; it links to its design; owner and agent move it on.
    assert!(page.contains("mstate proposed"), "{page}");
    assert_eq!(up("name=x&category=mockup&design_url=javascript:alert(1)", png.clone()).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(up("name=x&category=mockup&state=done", png.clone()).await.status(), StatusCode::BAD_REQUEST);
    assert!(up("name=Sidebar%20B&category=mockup&design_url=https%3A%2F%2Fclaude.ai%2Fartifact%2Fabc", png.clone()).await.status().is_success());
    let res = s
        .http
        .post(s.url("/p/vinculum/S-1/mockup-state"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("state", "approved")])
        .send()
        .await
        .unwrap();
    assert!(res.status().is_redirection());
    let page = get("/p/vinculum/S-1".into()).await.text().await.unwrap();
    assert!(page.contains("class=\"on\" aria-pressed=\"true\">approved"), "{page}");
    let (ok, r) = mcp_tool(&s, &token, &sess, "update_screenshot", json!({"id": "S-1", "state": "built"})).await;
    assert!(ok && r["mockup_state"] == "built", "{r}");
    let page = get("/p/vinculum/S-3".into()).await.text().await.unwrap();
    assert!(page.contains("https://claude.ai/artifact/abc"), "{page}");
}

#[tokio::test]
async fn projects_are_archived_and_restored() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let (ok, q) = mcp_tool(&s, &token, &sess, "ask_question", json!({"title": "Tabs or list?"})).await;
    assert!(ok, "{q}");
    let cookie = s.owner_cookie().await;
    let get = |path: &'static str| {
        let (http, url, cookie) = (s.http.clone(), s.url(path), cookie.clone());
        async move { http.get(url).header(COOKIE, cookie).send().await.unwrap().text().await.unwrap() }
    };
    let set = |status: &'static str| {
        let (http, url, cookie, origin) = (s.http.clone(), s.url("/p/vinculum/status"), cookie.clone(), s.base.clone());
        async move {
            http.post(url)
                .header(COOKIE, cookie)
                .header(ORIGIN, origin)
                .form(&[("status", status)])
                .send()
                .await
                .unwrap()
        }
    };
    let waiting = |s: &TestServer| {
        let db = s.state.db.clone();
        async move { db.call(|c| crate::waiting::count(c)).await.unwrap() }
    };
    assert_eq!(waiting(&s).await, 1);
    assert!(set("archived").await.status().is_redirection());
    assert_eq!(waiting(&s).await, 0, "an archived project waits for nothing");
    assert!(!get("/").await.contains("Tabs or list?"), "not on the Dashboard");
    let list = get("/projects").await;
    assert!(!list.contains("/p/vinculum\"") && list.contains("1 archived"), "{list}");
    assert!(get("/projects?archived=1").await.contains("/p/vinculum\""));
    assert!(get("/p/vinculum").await.contains("Restore it in Settings"));
    assert!(get("/p/vinculum?tab=settings").await.contains("Restore the project"));
    // Only the owner, and only these two.
    let res = s
        .http
        .post(s.url("/p/vinculum/status"))
        .bearer_auth(&token)
        .form(&[("status", "active")])
        .send()
        .await
        .unwrap();
    assert!(res.status().is_client_error());
    assert!(set("deleted").await.status().is_client_error());
    assert!(set("active").await.status().is_redirection());
    assert_eq!(waiting(&s).await, 1);
}

#[tokio::test]
async fn the_demo_project_shows_on_every_tab() {
    let s = TestServer::start().await;
    let data = s.state.config.data_dir();
    s.state.db.call(move |c| crate::demo::create(c, &data)).await.unwrap();
    let cookie = s.owner_cookie().await;
    for tab in ["", "?tab=questions", "?tab=tasks", "?tab=prs", "?tab=builds", "?tab=decisions", "?tab=screenshots"] {
        let res = s.http.get(s.url(&format!("/p/lumen-demo{tab}"))).header(COOKIE, &cookie).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{tab}");
    }
    let shots = s.http.get(s.url("/p/lumen-demo?tab=screenshots")).header(COOKIE, &cookie).send().await.unwrap();
    assert!(shots.text().await.unwrap().contains("Album sidebar, variant A"));
    let img = s.http.get(s.url("/p/lumen-demo/S-1/image")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(img.headers()["content-type"], "image/png");
}

#[tokio::test]
async fn the_owner_edits_a_projects_details() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let cookie = s.owner_cookie().await;
    let save = |fields: Vec<(&'static str, &'static str)>| {
        let (http, url, cookie, origin) =
            (s.http.clone(), s.url("/p/vinculum/settings"), cookie.clone(), s.base.clone());
        async move {
            let mut form: Vec<(&str, &str)> = vec![
                ("name", "Lituus"),
                ("description", "Settings app"),
                ("repo", "giosuel/vinculum"),
                ("checkout", "~/vinculum"),
                ("machine", "giosuel.ch"),
                ("stack", "Rust, Slint ,, zbus"),
                ("platform", "Linux"),
                ("model", "Claude Opus 5.5"),
                ("effort", "high"),
                ("pm_rules", "One rule\n\nAnother"),
                ("git_rules", "DCO"),
                ("ports", "21300 demo"),
                ("build_product", ""),
            ];
            for (k, v) in fields {
                form.retain(|(a, _)| *a != k);
                form.push((k, v));
            }
            let res = http.post(url).header(COOKIE, cookie).header(ORIGIN, origin).form(&form).send().await.unwrap();
            res.headers()[LOCATION].to_str().unwrap().to_owned()
        }
    };
    assert_eq!(save(vec![]).await, "/p/vinculum?tab=settings&settings=saved");
    let p = s.state.db.call(|c| crate::projects::require(c, "vinculum")).await.unwrap();
    assert_eq!((p.name.as_str(), p.repo.as_str()), ("Lituus", "giosuel/vinculum"));
    // Stack and rules live in the project document now; the form doesn't touch them.
    assert!(p.stack.is_empty() && p.pm_rules.is_empty());
    assert_eq!(p.ports, ["21300 demo"]);
    assert_eq!(save(vec![("name", " ")]).await, "/p/vinculum?tab=settings&settings=name");
    assert_eq!(save(vec![("repo", "not a repo")]).await, "/p/vinculum?tab=settings&settings=repo");
    let page =
        s.http.get(s.url("/p/vinculum?tab=settings&settings=repo")).header(COOKIE, &cookie).send().await.unwrap();
    let html = page.text().await.unwrap();
    assert!(html.contains("owner/name") && html.contains(r#"value="Lituus""#), "{html}");
    // Only the owner.
    let res =
        s.http.post(s.url("/p/vinculum/settings")).bearer_auth(&token).form(&[("name", "x")]).send().await.unwrap();
    assert!(res.status().is_client_error());
}

/// git in a folder, for the tests that need repositories.
fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// The commits on top of `base` in `work`, as `flow contribute` sends them.
fn bundle_b64(work: &std::path::Path, base: &str) -> String {
    use base64::Engine as _;
    let file = work.join("draft.bundle");
    git_in(work, &["bundle", "create", "-q", file.to_str().unwrap(), &format!("{base}..HEAD")]);
    base64::engine::general_purpose::STANDARD.encode(std::fs::read(&file).unwrap())
}

#[tokio::test]
async fn drafts_for_other_repositories_go_out_only_when_the_owner_sends_them() {
    let s = TestServer::start().await;
    let agent = s.token(TokenKind::Project, Some("imaginarium")).await;
    let other = s.token(TokenKind::Project, Some("vinculum")).await;
    let cookie = s.owner_cookie().await;

    // "GitHub": their repository (with a PR template) and the owner's fork, as bare repositories.
    let gh = s.state.config.data_dir().join("gh");
    let theirs = gh.join("ophymx/formulary.git");
    let fork = gh.join("giosuel/formulary.git");
    for dir in [&theirs, &fork] {
        std::fs::create_dir_all(dir).unwrap();
        git_in(dir, &["init", "-q", "--bare"]);
    }
    let up = s.state.config.data_dir().join("upstream-work");
    std::fs::create_dir_all(up.join(".github")).unwrap();
    git_in(&up, &["init", "-q"]);
    std::fs::write(up.join(".github/pull_request_template.md"), "## Summary\n\n## Testing\n").unwrap();
    std::fs::write(up.join("fence.rs"), "let shift = h / 2.0;\n").unwrap();
    git_in(&up, &["add", "."]);
    git_in(&up, &["commit", "-q", "-m", "init"]);
    git_in(&up, &["push", "-q", theirs.to_str().unwrap(), "HEAD:refs/heads/main"]);
    let base = git_in(&up, &["rev-parse", "HEAD"]);
    // The agent's commit on top.
    std::fs::write(up.join("fence.rs"), "let shift = center - axis;\n").unwrap();
    git_in(&up, &["commit", "-q", "-am", "Center fences on the math axis"]);

    let post_draft = |token: String, body: Value| {
        let s = &s;
        async move { s.http.post(s.url("/api/contributions")).bearer_auth(token).json(&body).send().await.unwrap() }
    };
    let pr = |revise: &str, text: &str| {
        json!({"type": "pr", "repo": "ophymx/formulary", "base": "main", "branch": "center-fences",
               "title": "Center fence delimiters", "body": text, "why": "Tall parentheses sat low.",
               "revise": revise, "bundle": bundle_b64(&up, &base)})
    };
    let res = post_draft(agent.clone(), pr("", "## Summary\nCenters them.")).await;
    assert_eq!(res.status(), StatusCode::OK, "{}", res.text().await.unwrap());
    // Nothing went out; the owner has something waiting.
    assert!(s.poster.posted.lock().unwrap().is_empty());
    assert_eq!(git_in(&fork, &["for-each-ref"]), "", "nothing pushed");
    let count = s.db.call(|c| crate::waiting::count(c)).await.unwrap();
    assert_eq!(count, 1);

    // The page shows what AgentFlow read from the bundle, and the checks.
    let page =
        s.http.get(s.url("/p/imaginarium/C-1")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Send the PR") && page.contains("+let shift = center - axis;"), "{page}");
    assert!(page.contains("has headings the text doesn") && page.contains("t: Testing"), "the template check");
    assert!(page.contains("No decision of yours is linked"));
    let fp = page.split("name=\"fp\" value=\"").nth(1).unwrap().split('"').next().unwrap().to_owned();
    let form = |title: &str, note: &str, fp: &str| {
        vec![
            ("title", title.to_owned()),
            ("body", "## Summary\nCenters them.\n\n## Testing\nA test.".to_owned()),
            ("note", note.to_owned()),
            ("fp", fp.to_owned()),
        ]
    };
    let owner_post = |path: &str, f: Vec<(&'static str, String)>| {
        let s = &s;
        let (path, cookie) = (path.to_owned(), cookie.clone());
        async move {
            let res = s
                .http
                .post(s.url(&path))
                .header(COOKIE, &cookie)
                .header(ORIGIN, &s.base)
                .form(&f)
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::SEE_OTHER);
            res.headers()[LOCATION].to_str().unwrap().to_owned()
        }
    };

    // Only the owner presses the buttons; another project can't touch the draft.
    let res = s
        .http
        .post(s.url("/p/imaginarium/C-1/send"))
        .bearer_auth(&agent)
        .form(&form("x", "", &fp))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = post_draft(other.clone(), pr("C-1", "mine now")).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Ask for changes: the note and the owner's edits reach the agent.
    assert_eq!(
        owner_post("/p/imaginarium/C-1/changes", form("Center fences", "", &fp)).await,
        "/p/imaginarium/C-1?draft=nonote"
    );
    assert_eq!(
        owner_post("/p/imaginarium/C-1/changes", form("Center fences", "add a test", &fp)).await,
        "/p/imaginarium/C-1?draft=asked"
    );
    let ts = mcp_session(&s, &agent).await;
    let (_, news) = mcp_tool(&s, &agent, &ts, "since_last_check", json!({})).await;
    assert!(news.to_string().contains("add a test"), "{news}");
    assert_eq!(s.db.call(|c| crate::waiting::count(c)).await.unwrap(), 0, "back with the agent");
    let r =
        s.db.call(|c| {
            Ok(crate::records::try_find(c, &crate::records::RecordRef::parse("imaginarium/C-1")?, None)?.unwrap())
        })
        .await
        .unwrap();
    assert_eq!(r.title, "Center fences", "the owner's edit is kept");

    // A new version; the old page's fingerprint no longer sends anything.
    let res = post_draft(agent.clone(), pr("C-1", "## Summary\nCenters them.\n\n## Testing\nA test.")).await;
    assert_eq!(res.status(), StatusCode::OK, "{}", res.text().await.unwrap());
    assert_eq!(
        owner_post("/p/imaginarium/C-1/send", form("Center fences", "", &fp)).await,
        "/p/imaginarium/C-1?draft=changed"
    );
    assert!(s.poster.posted.lock().unwrap().is_empty());

    // Send: pushed to the fork, the PR opened with the owner's words, watched from now on.
    let page =
        s.http.get(s.url("/p/imaginarium/C-1")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    let fp2 = page.split("name=\"fp\" value=\"").nth(1).unwrap().split('"').next().unwrap().to_owned();
    assert_eq!(
        owner_post("/p/imaginarium/C-1/send", form("Center fence delimiters on the axis", "", &fp2)).await,
        "/p/imaginarium/C-1?draft=sent"
    );
    let head = git_in(&up, &["rev-parse", "HEAD"]);
    assert_eq!(git_in(&fork, &["rev-parse", "refs/heads/center-fences"]), head);
    match &s.poster.posted.lock().unwrap()[0] {
        crate::outgoing::Post::Pr { repo, base, head, title, body } => {
            assert_eq!(
                (repo.as_str(), base.as_str(), head.as_str()),
                ("ophymx/formulary", "main", "giosuel:center-fences")
            );
            assert_eq!(title, "Center fence delimiters on the axis");
            assert!(body.contains("## Testing"));
        }
        p => panic!("{p:?}"),
    }
    let r =
        s.db.call(|c| {
            Ok(crate::records::try_find(c, &crate::records::RecordRef::parse("imaginarium/C-1")?, None)?.unwrap())
        })
        .await
        .unwrap();
    assert_eq!(r.state, "sent");
    assert_eq!(r.title, "ophymx/formulary#41: Center fence delimiters on the axis");
    assert_eq!(r.str("fork"), "giosuel/formulary");
    // A second press sends nothing more.
    assert_eq!(owner_post("/p/imaginarium/C-1/send", form("x", "", &fp2)).await, "/p/imaginarium/C-1?draft=changed");
    assert_eq!(s.poster.posted.lock().unwrap().len(), 1);

    // A reply to a maintainer, through MCP; then new commits on top of the PR's branch.
    let (ok, r) = mcp_tool(
        &s,
        &agent,
        &ts,
        "draft_contribution",
        json!({"type": "comment", "on": "C-1", "body": "Yes, still on 1.17."}),
    )
    .await;
    assert!(ok, "{r}");
    let page =
        s.http.get(s.url("/p/imaginarium/C-1")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Post the comment"));
    let fp3 = page.split("name=\"fp\" value=\"").nth(1).unwrap().split('"').next().unwrap().to_owned();
    owner_post("/p/imaginarium/C-1/send", vec![("body", "Yes, still on 1.17.0.".into()), ("fp", fp3)]).await;
    assert_eq!(
        s.poster.posted.lock().unwrap()[1],
        crate::outgoing::Post::Comment {
            repo: "ophymx/formulary".into(),
            number: 41,
            body: "Yes, still on 1.17.0.".into()
        }
    );
    std::fs::write(up.join("fence.rs"), "let shift = center - axis; // rule 19\n").unwrap();
    git_in(&up, &["commit", "-q", "-am", "Cite the rule"]);
    let res = post_draft(
        agent.clone(),
        json!({"type": "commits", "on": "C-1", "body": "", "bundle": bundle_b64(&up, &head)}),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "{}", res.text().await.unwrap());
    let page =
        s.http.get(s.url("/p/imaginarium/C-1")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Push the commits") && page.contains("Cite the rule"));
    let fp4 = page.split("name=\"fp\" value=\"").nth(1).unwrap().split('"').next().unwrap().to_owned();
    owner_post("/p/imaginarium/C-1/send", vec![("body", String::new()), ("fp", fp4)]).await;
    assert_eq!(git_in(&fork, &["rev-parse", "refs/heads/center-fences"]), git_in(&up, &["rev-parse", "HEAD"]));
    assert_eq!(s.poster.posted.lock().unwrap().len(), 2, "no note, no comment");

    // An issue the owner drops: nothing is posted, the agent hears it.
    let (ok, r) = mcp_tool(
        &s,
        &agent,
        &ts,
        "draft_contribution",
        json!({"type": "issue", "repo": "loro-dev/loro", "title": "Slow redo", "body": "Redo gets slower."}),
    )
    .await;
    assert!(ok, "{r}");
    let id = r["id"].as_str().unwrap().split('/').nth(1).unwrap().to_owned();
    owner_post(&format!("/p/imaginarium/{id}/drop"), vec![("note", "not yet".into())]).await;
    let (_, news) = mcp_tool(&s, &agent, &ts, "since_last_check", json!({})).await;
    assert!(news.to_string().contains("not yet"), "{news}");
    assert_eq!(s.poster.posted.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn jobs_run_on_a_build_server_and_bring_back_what_was_asked() {
    use std::os::unix::fs::PermissionsExt;
    // A stand-in for ssh: runs the command here, in a home of its own, with systemd-run and
    // systemctl that just run the command / do nothing.
    let s = TestServer::start_with(|c, data| {
        let (home, bin) = (data.join("hq-home"), data.join("hq-bin"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let write = |name: &str, text: String| {
            let p = bin.join(name);
            std::fs::write(&p, text).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        write(
            "ssh",
            format!(
                "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in -o|-F) shift 2;; -*) shift;; *) break;; esac; done\nshift\ncd {h} || exit 255\nexport HOME={h} PATH={b}:$PATH\nexec sh -c \"$*\"\n",
                h = home.display(),
                b = bin.display()
            ),
        );
        write("systemd-run", "#!/bin/sh\nwhile [ \"$1\" != \"--\" ]; do shift; done\nshift\nexec \"$@\"\n".into());
        write("systemctl", "#!/bin/sh\nexit 0\n".into());
        write("nvidia-smi", "#!/bin/sh\nexit 1\n".into());
        c.build_servers = vec![crate::config::BuildServer {
            name: "hq".into(),
            ssh: "hq".into(),
            ssh_program: bin.join("ssh").display().to_string(),
            min_threads: 0.0,
            min_memory_gb: 0.0,
            reserve_memory_gb: 0.0,
            ..Default::default()
        }];
    })
    .await;
    crate::remote::spawn(s.state.clone());
    let agent = s.token(TokenKind::Project, Some("imaginarium")).await;
    let other = s.token(TokenKind::Project, Some("vinculum")).await;
    // The server from the config is in the list once the scheduler has started.
    for _ in 0..100 {
        if !crate::remote::servers(&s.state).is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // The agent's checkout: a commit, then a change and a new file not committed yet.
    let work = s.state.config.data_dir().join("checkout");
    std::fs::create_dir_all(work.join("crate")).unwrap();
    git_in(&work, &["init", "-q"]);
    std::fs::write(work.join("a.txt"), "committed\n").unwrap();
    std::fs::write(work.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(work.join("crate/c.txt"), "c\n").unwrap();
    git_in(&work, &["add", "."]);
    git_in(&work, &["commit", "-q", "-m", "init"]);
    git_in(&work, &["tag", "v0.1.0"]);
    std::fs::write(work.join("a.txt"), "changed, not committed\n").unwrap();
    std::fs::write(work.join("new.txt"), "new\n").unwrap();
    std::fs::create_dir_all(work.join("target")).unwrap();
    std::fs::write(work.join("target/junk"), "never sent").unwrap();

    let cmd = "cat ../a.txt ../new.txt; git describe --tags; echo 'warning: careful'; ls ../target 2>&1 | head -1; mkdir -p ../../target/release && echo built > ../../target/release/app";
    let start = |dir: std::path::PathBuf, token: String| {
        let s = &s;
        async move {
            s.http
                .post(s.url("/api/remote/jobs"))
                .bearer_auth(token)
                .json(&json!({"dir": dir, "command": ["sh", "-c", cmd], "fetch": ["target/release/app"]}))
                .send()
                .await
                .unwrap()
        }
    };
    // A project without a build server builds where its agent is.
    let res = start(work.join("crate"), agent.clone()).await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    s.db.call(|c| {
        let p = crate::projects::get(c, "imaginarium")?.unwrap();
        crate::remote::set_project_server(c, p.id, "hq")
    })
    .await
    .unwrap();
    let res = start(work.join("crate"), agent.clone()).await;
    assert_eq!(res.status(), StatusCode::OK, "{}", res.text().await.unwrap());
    let id = res.json::<Value>().await.unwrap()["id"].as_i64().unwrap();

    // Another project's agent can't read or stop it.
    let res = s.http.get(s.url(&format!("/api/remote/jobs/{id}/output"))).bearer_auth(&other).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Follow it as the CLI does.
    let mut text = String::new();
    let mut after = 0;
    let last = loop {
        let o: Value = s
            .http
            .get(s.url(&format!("/api/remote/jobs/{id}/output?after={after}")))
            .bearer_auth(&agent)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        text.push_str(o["text"].as_str().unwrap());
        after = o["next"].as_u64().unwrap();
        if o["finished"].as_bool().unwrap() && o["text"].as_str().unwrap().is_empty() {
            break o;
        }
        assert!(text.len() < 1_000_000);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    };
    assert_eq!(last["state"], "done", "{last}\n{text}");
    assert!(
        text.contains("changed, not committed") && text.contains("new\n"),
        "uncommitted changes went over:\n{text}"
    );
    assert!(text.contains("v0.1.0"), "tags went over:\n{text}");
    assert!(text.contains("No such file") || !text.contains("junk"), "ignored files stay here:\n{text}");
    assert_eq!(std::fs::read_to_string(work.join("target/release/app")).unwrap(), "built\n", "brought back");
    assert_eq!(last["warnings"], 1);

    // The Log has every line, the warning marked, and the job's own entry.
    let rows: Vec<(String, String, String)> =
        s.db.call(|c| {
            let mut st = c.prepare("SELECT action, severity, summary FROM log WHERE via = 'build' ORDER BY seq")?;
            let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
            Ok(r)
        })
        .await
        .unwrap();
    assert!(rows.iter().any(|(a, sev, t)| a == "output" && sev == "warn" && t == "warning: careful"), "{rows:?}");
    assert!(rows.iter().any(|(a, _, t)| a == "run" && t.contains(" · ok · ")), "{rows:?}");
    let detail: String =
        s.db.call(|c| {
            Ok(c.query_row("SELECT detail FROM log WHERE via = 'build' AND detail IS NOT NULL", [], |r| r.get(0))?)
        })
        .await
        .unwrap();
    let lines: Vec<String> = serde_json::from_str::<Value>(&detail).unwrap()["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect();
    let mut unique = lines.clone();
    unique.dedup();
    assert_eq!(lines.len(), unique.len(), "no line twice: {lines:?}");
    assert!(lines.iter().any(|l| l == "warning: careful"));

    // A second run with nothing changed: nothing there is rewritten (so nothing is rebuilt).
    let there = s.state.config.data_dir().join("hq-home/agentflow/imaginarium/work");
    let mtime = |f: &str| std::fs::metadata(there.join(f)).unwrap().modified().unwrap();
    let before = (mtime("a.txt"), mtime("crate/c.txt"), mtime("new.txt"));
    // And a file deleted here goes there too.
    std::fs::remove_file(work.join("new.txt")).unwrap();
    let res = start(work.clone(), agent.clone()).await;
    let id2 = res.json::<Value>().await.unwrap()["id"].as_i64().unwrap();
    for _ in 0..200 {
        let j = s.db.call(move |c| crate::remote::get(c, id2)).await.unwrap().unwrap();
        if j.is_finished() {
            assert_eq!(j.state, "done", "{}", j.reason);
            assert_eq!(
                (mtime("a.txt"), mtime("crate/c.txt")),
                (before.0, before.1),
                "unchanged files keep their times"
            );
            assert!(!there.join("new.txt").exists(), "deleted here, deleted there");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // Offline: refused at once, so the agent builds here.
    s.state.remote.lock().unwrap().servers.get_mut("hq").unwrap().probe.online = false;
    let res = start(work.clone(), agent.clone()).await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn build_servers_are_added_in_settings_and_picked_per_project() {
    let s = TestServer::start().await;
    crate::remote::spawn(s.state.clone());
    let cookie = s.owner_cookie().await;
    s.token(TokenKind::Project, Some("vinculum")).await;
    let post = |path: &str, form: Vec<(&'static str, String)>| {
        let s = &s;
        let (path, cookie) = (path.to_owned(), cookie.clone());
        async move {
            let res = s
                .http
                .post(s.url(&path))
                .header(COOKIE, &cookie)
                .header(ORIGIN, &s.base)
                .form(&form)
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::SEE_OTHER, "{path}");
            res.headers()[LOCATION].to_str().unwrap().to_owned()
        }
    };
    // Bad names and targets are refused.
    assert!(
        post("/settings/build-servers", vec![("name", "a b".into()), ("ssh", "builder@10.0.8.2".into())])
            .await
            .contains("error-name")
    );
    assert!(
        post("/settings/build-servers", vec![("name", "hq".into()), ("ssh", "-oProxyCommand=x".into())])
            .await
            .contains("error-target")
    );
    // Added with a key AgentFlow makes: the card shows the line to add over there.
    let to = post(
        "/settings/build-servers",
        vec![("name", "hq".into()), ("ssh", "builder@10.0.8.2".into()), ("make_key", "1".into())],
    )
    .await;
    assert!(to.contains("bs=added"), "{to}");
    let key = s.state.config.data_dir().join("remote/keys/hq");
    assert!(key.exists() && key.with_extension("pub").exists());
    let page =
        s.http.get(s.url("/settings?bs=added")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("restrict ssh-ed25519") && page.contains("builder@10.0.8.2"), "the key line");
    assert_eq!(crate::remote::servers(&s.state)[0].key_file, key.display().to_string());
    // A project picks it in its Settings; the Settings card lists the project.
    let page = s
        .http
        .get(s.url("/p/vinculum?tab=settings"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("<option value=\"hq\""));
    let form = vec![
        ("name", "Vinculum".into()),
        ("description", String::new()),
        ("repo", String::new()),
        ("checkout", String::new()),
        ("machine", String::new()),
        ("stack", String::new()),
        ("platform", String::new()),
        ("model", String::new()),
        ("effort", String::new()),
        ("pm_rules", String::new()),
        ("git_rules", String::new()),
        ("ports", String::new()),
        ("build_product", String::new()),
        ("build_server", "hq".into()),
    ];
    assert!(post("/p/vinculum/settings", form).await.contains("settings=saved"));
    let pick =
        s.db.call(|c| crate::remote::project_server(c, crate::projects::get(c, "vinculum")?.unwrap().id))
            .await
            .unwrap();
    assert_eq!(pick.as_deref(), Some("hq"));
    let page = s.http.get(s.url("/settings")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Used by Vinculum"));
    // Limits, then removal: the key goes, the project builds locally again.
    assert!(
        post(
            "/settings/build-servers/hq/limits",
            vec![
                ("min_threads", "4".into()),
                ("min_memory_gb", "8".into()),
                ("reserve_memory_gb", "4".into()),
                ("keep_days", "7".into())
            ]
        )
        .await
        .contains("bs=saved")
    );
    assert_eq!(crate::remote::servers(&s.state)[0].min_threads, 4.0);
    assert!(post("/settings/build-servers/hq/remove", vec![]).await.contains("bs=removed"));
    assert!(crate::remote::servers(&s.state).is_empty() && !key.exists());
    let pick =
        s.db.call(|c| crate::remote::project_server(c, crate::projects::get(c, "vinculum")?.unwrap().id))
            .await
            .unwrap();
    assert_eq!(pick, None);
}

#[tokio::test]
async fn agents_can_go_ahead_with_a_default_while_a_question_waits() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("imaginarium")).await;
    let cookie = s.owner_cookie().await;
    let ms = mcp_session(&s, &token).await;
    let ask = |title: &str, going_ahead: bool, recommended: Option<u32>| {
        json!({"title": title, "body": "b", "options": [{"title": "Icons only"}, {"title": "Icons with labels"}],
               "recommended": recommended, "going_ahead": going_ahead})
    };
    let (ok, _) = mcp_tool(&s, &token, &ms, "ask_question", ask("No default", true, None)).await;
    assert!(!ok, "going ahead needs a recommendation");
    let (ok, r) = mcp_tool(&s, &token, &ms, "ask_question", ask("Sidebar labels?", true, Some(2))).await;
    assert!(ok, "{r}");
    let (ok, _) = mcp_tool(&s, &token, &ms, "ask_question", ask("Second one", true, Some(2))).await;
    assert!(ok);
    // Not in the badge; in its own group on the Dashboard; the question's page says so.
    assert_eq!(s.db.call(|c| crate::waiting::count(c)).await.unwrap(), 0);
    let dash = s.http.get(s.url("/")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(
        dash.contains("Going ahead unless you say otherwise") && dash.contains("going ahead with 2: Icons with labels")
    );
    let page =
        s.http.get(s.url("/p/imaginarium/Q-1")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("is going ahead with <strong>2: Icons with labels</strong>"));
    // Confirming the default, and changing the other: the agent hears which.
    for (q, option) in [("Q-1", "2"), ("Q-2", "1")] {
        let res = s
            .http
            .post(s.url(&format!("/p/imaginarium/{q}/answer")))
            .header(COOKIE, &cookie)
            .header(ORIGIN, &s.base)
            .form(&[("option", option)])
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
    }
    let (_, news) = mcp_tool(&s, &token, &ms, "since_last_check", json!({})).await;
    let news = news.to_string();
    assert!(news.contains("Owner confirmed Q-1: keep 2: Icons with labels"), "{news}");
    assert!(
        news.contains(
            "Owner changed Q-2: 1: Icons only (not the default you went ahead with, 2: Icons with labels); switch to it"
        ),
        "{news}"
    );
}

#[tokio::test]
async fn devices_download_flow() {
    let dir = tempfile::tempdir().unwrap();
    let flow = dir.path().join("flow");
    std::fs::write(&flow, b"\x7fELF flow").unwrap();
    let f = flow.clone();
    let s = TestServer::start_with(move |c, _| c.flow_bin = f.display().to_string()).await;
    // Public, like the install script: a machine gets its token only by flow connect.
    for path in ["/download/flow", "/download/agentflow"] {
        let res = s.http.get(s.url(path)).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        assert!(res.headers()["content-disposition"].to_str().unwrap().contains("\"flow\""));
        assert_eq!(res.bytes().await.unwrap().as_ref(), b"\x7fELF flow", "flow, not the server");
    }
    let sum = s.http.get(s.url("/download/flow.sha256")).send().await.unwrap().text().await.unwrap();
    assert_eq!(sum.trim(), crate::builds::sha256_of(&b"\x7fELF flow"[..]).unwrap().0);
}

/// The owner, 2026-11-12: agents "freely add/edit/delete(archive)" milestones, steps, tasks,
/// decisions and questions; archived ones stay reachable when asked for.
#[tokio::test]
async fn milestones_steps_and_tasks_are_edited_and_archived_freely() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let call = |name: &'static str, args: Value| {
        let (s, token, sess) = (&s, token.clone(), sess.clone());
        async move { mcp_tool(s, &token, &sess, name, args).await }
    };
    assert!(call("add_milestone", json!({"name": "P9", "title": "Prep"})).await.0);
    assert!(call("add_milestone", json!({"name": "P10", "title": "More prep"})).await.0);
    for (n, t) in [("P9.3", "Agents"), ("P9.3b", "Batches"), ("P9.3c", "Asking first")] {
        assert!(call("add_milestone", json!({"name": n, "title": t, "step_of": "P9"})).await.0);
    }
    for n in ["P9.3", "P9.3b", "P9.3c"] {
        let (ok, r) = call("update_milestone", json!({"milestone": n, "state": "closed", "summary": format!("{n} done.")})).await;
        assert!(ok, "{r}");
    }
    let (ok, r) = call("add_tasks", json!({"milestone": "P9.3b", "tasks": [{"title": "Batch API", "state": "done"}]})).await;
    assert!(ok && r["ids"] == json!(["T-1"]), "{r}");
    let (ok, r) = call("add_tasks", json!({"milestone": "P9.3c", "tasks": [{"title": "Ask UI", "state": "done"}]})).await;
    assert!(ok && r["ids"] == json!(["T-2"]), "{r}");
    let (ok, n) = call("add_note", json!({"name": "Batch log", "category": "log", "text": "ok", "record": "P9.3b"})).await;
    assert!(ok, "{n}");
    // Fold P9.3b into P9.3: a done task with its dates, then the step archived with move_to.
    let (ok, r) = call("add_tasks", json!({"milestone": "P9.3", "tasks": [
        {"title": "P9.3b: Batches", "state": "done", "body": "P9.3b done.", "started_at": "2026-11-09", "done_at": "2026-11-09 18:00"}]})).await;
    assert!(ok, "{r}");
    assert!(!call("add_tasks", json!({"milestone": "P9.3", "tasks": [{"title": "x", "state": "todo", "estimate": "1 h", "done_at": "2026-11-09"}]})).await.0);
    assert!(!call("archive_milestone", json!({"milestone": "P9.3b", "reason": ""})).await.0, "a reason");
    let (ok, d) = call("archive_milestone", json!({"milestone": "P9.3b", "move_to": "P9.3", "reason": "folded into P9.3"})).await;
    assert!(ok, "{d}");
    assert_eq!(d["moved"], json!(["T-1", "N-1"]));
    // Without move_to, its tasks are archived with it, and restoring brings them back.
    let (ok, d) = call("archive_milestone", json!({"milestone": "P9.3c", "reason": "not needed"})).await;
    assert!(ok && d["archived_with_it"] == json!(["T-2"]), "{d}");
    let (_, t2) = call("get_record", json!({"id": "T-2"})).await;
    assert_eq!(t2["state"], "archived", "{t2}");
    let (ok, err) = call("update_milestone", json!({"milestone": "P9.3c", "title": "x"})).await;
    assert!(!ok && err.as_str().unwrap().contains("archived"), "{err}");
    let (ok, err) = call("add_milestone", json!({"name": "P9.3c", "title": "Again", "step_of": "P9"})).await;
    assert!(!ok && err.as_str().unwrap().contains("names stay taken"), "archived names stay taken: {err}");
    let (_, found) = call("find_records", json!({"kind": "task", "milestone": "P9"})).await;
    assert_eq!(found["count"], 2, "archived left out: {found}");
    let (_, found) = call("find_records", json!({"kind": "task", "milestone": "P9", "include_archived": true})).await;
    assert_eq!(found["count"], 3, "{found}");
    // An archived step reads back when asked for.
    let (ok, m) = call("get_milestone", json!({"milestone": "P9.3c"})).await;
    assert!(!ok, "{m}");
    let (ok, m) = call("get_milestone", json!({"milestone": "P9.3c", "include_archived": true})).await;
    assert!(ok && m["summary"] == "P9.3c done." && m["archived"]["reason"] == "not needed" && m["tasks"][0]["id"] == "T-2", "{m}");
    // A concluded step in one call, as history.
    let (ok, m) = call("add_milestone", json!({"name": "P9.9", "title": "Old", "step_of": "P9", "state": "closed",
        "started_at": "2026-11-01", "closed_at": "2026-11-02", "summary": "Done long ago."})).await;
    assert!(ok && m["state"] == "closed", "{m}");
    let (_, m) = call("get_milestone", json!({"milestone": "P9.9"})).await;
    assert!(m["closed_at"].as_str().unwrap().starts_with("2026-11-02") && m["summary"] == "Done long ago.", "{m}");
    let (ok, r) = call("restore_milestone", json!({"milestone": "P9.3c"})).await;
    assert!(ok && r["with"] == json!(["T-2"]), "{r}");
    let (_, t2) = call("get_record", json!({"id": "T-2"})).await;
    assert_eq!(t2["state"], "done", "{t2}");
    // Steps and milestones edit freely: reopened, moved to another milestone, summary rewritten.
    assert!(call("update_milestone", json!({"milestone": "P9.3", "summary": "Agents drive the app; batches."})).await.0);
    let (ok, r) = call("add_task", json!({"milestone": "P10", "title": "Later", "estimate": "1 h"})).await;
    assert!(ok, "{r}");
    assert!(call("update_task", json!({"id": "T-4", "milestone": "P9.3"})).await.0);
    assert!(call("update_milestone", json!({"milestone": "P9.3", "state": "active"})).await.0);
    assert!(call("update_milestone", json!({"milestone": "P9.3", "step_of": "P10"})).await.0);
    // Tasks archive with a reason and come back with any other state.
    assert!(!call("update_task", json!({"id": "T-4", "state": "archived"})).await.0, "a reason");
    let (ok, r) = call("update_task", json!({"id": "T-4", "state": "archived", "note": "a duplicate of T-1"})).await;
    assert!(ok, "{r}");
    let tasks = s.http.get(s.url("/p/vinculum?tab=tasks")).header(COOKIE, s.owner_cookie().await).send().await.unwrap().text().await.unwrap();
    assert!(!tasks.contains("Later"), "{tasks}");
    assert!(call("update_task", json!({"id": "T-4", "state": "todo"})).await.0);
    // A milestone takes its steps, tasks and the decisions and questions scoped to it along.
    assert!(call("update_milestone", json!({"milestone": "P10", "state": "active"})).await.0);
    let (ok, d) = call("record_decision", json!({"title": "Keep it small", "owner_words": "small", "source": "owner"})).await;
    assert!(ok, "{d}");
    let (ok, d) = call("record_decision", json!({"title": "Always ask first", "owner_words": "ask", "source": "owner", "scope": "project"})).await;
    assert!(ok, "{d}");
    let (ok, a) = call("archive_milestone", json!({"milestone": "P10", "reason": "dropped"})).await;
    assert!(ok, "{a}");
    assert_eq!(a["steps"], json!(["P9.3"]));
    assert!(a["archived_with_it"].as_array().unwrap().iter().any(|x| x == "D-1"), "{a}");
    let (_, d2) = call("get_record", json!({"id": "D-2"})).await;
    assert_eq!(d2["state"], "current", "project-wide stays: {d2}");
    assert!(!call("restore_milestone", json!({"milestone": "P9.3"})).await.0, "restore its milestone");
    let (ok, r) = call("restore_milestone", json!({"milestone": "P10"})).await;
    assert!(ok, "{r}");
    let (_, d1) = call("get_record", json!({"id": "D-1"})).await;
    assert_eq!(d1["state"], "current", "{d1}");
    let log: String = s
        .state
        .db
        .call(|c| Ok(c.query_row("SELECT group_concat(summary, ' | ') FROM events WHERE kind LIKE 'milestone.%'", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert!(log.contains("Step P9.3b archived (Batches): folded into P9.3; moved to P9.3: T-1, N-1"), "{log}");
}

/// Scopes (the owner, 2026-11-12, and Imaginarium's review): defaults, changing them, batches,
/// what agents see by default, news in replies and again.
#[tokio::test]
async fn questions_and_decisions_have_scopes() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let call = |name: &'static str, args: Value| {
        let (s, token, sess) = (&s, token.clone(), sess.clone());
        async move { mcp_tool(s, &token, &sess, name, args).await }
    };
    let hello = json!({"session": "V", "machine": "m", "model": "o", "effort": "h"});
    assert!(call("hello", hello.clone()).await.0);
    for (n, t) in [("M8", "Done"), ("P9", "Prep"), ("M9", "Next")] {
        assert!(call("add_milestone", json!({"name": n, "title": t})).await.0);
    }
    assert!(call("add_milestone", json!({"name": "P9.1", "title": "Step", "step_of": "P9"})).await.0);
    assert!(call("add_milestone", json!({"name": "M9.1", "title": "Step", "step_of": "M9"})).await.0);
    assert!(call("update_milestone", json!({"milestone": "P9", "state": "active"})).await.0);
    assert!(call("update_milestone", json!({"milestone": "M9", "state": "active"})).await.0);
    // Two active: the one whose step has a task in doing.
    assert!(call("add_task", json!({"milestone": "P9.1", "title": "Work", "estimate": "1 h", "state": "doing"})).await.0);
    let (_, q) = call("ask_question", json!({"title": "Which?"})).await;
    assert_eq!(q["scope"], "P9", "{q}");
    // A step names its milestone; "project"; a planned milestone.
    let (_, q) = call("ask_question", json!({"title": "Step?", "scope": "M9.1"})).await;
    assert_eq!(q["scope"], "M9");
    let (_, q) = call("ask_question", json!({"title": "Everywhere?", "scope": "project", "related": ["D-9"]})).await;
    assert!(q.as_str().is_some_and(|e| e.contains("D-9")), "related must be decisions: {q}");
    let (ok, d) = call("record_decision", json!({"title": "Old rule", "owner_words": "old", "source": "owner", "scope": "M8"})).await;
    assert!(ok, "{d}");
    let (ok, q) = call("ask_question", json!({"title": "Everywhere?", "scope": "project", "related": ["D-1"]})).await;
    assert!(ok && q["scope"] == "project", "{q}");
    // Questions: scope changed, several at once; archived ones unblock and come back.
    let (ok, r) = call("update_question", json!({"ids": ["Q-1", "Q-2"], "scope": "M9", "reason": "about M9"})).await;
    assert!(ok && r["questions"].as_array().unwrap().iter().all(|q| q["scope"] == "M9"), "{r}");
    let (ok, q4) = call("ask_question", json!({"title": "Block?", "blocks": ["T-1"]})).await;
    assert!(ok, "{q4}");
    let (ok, r) = call("update_question", json!({"id": "Q-4", "state": "archived", "reason": "not needed"})).await;
    assert!(ok && r["state"] == "archived", "{r}");
    let (_, t1) = call("get_record", json!({"id": "T-1"})).await;
    assert_eq!(t1["state"], "todo", "archiving unblocks: {t1}");
    let (ok, r) = call("update_question", json!({"id": "Q-4", "state": "open", "reason": "needed after all"})).await;
    assert!(ok && r["state"] == "open", "{r}");
    // The owner moves an open question's scope on its page before answering.
    let cookie = s.owner_cookie().await;
    let page = s.http.get(s.url("/p/vinculum/Q-3")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Its decision goes into") && page.contains("Project-wide"), "{page}");
    let m9: i64 = s
        .db
        .call(|c| {
            let p = crate::projects::require(c, "vinculum")?;
            Ok(crate::projects::find_milestone(c, p.id, "M9")?.id)
        })
        .await
        .unwrap();
    let res = s.http.post(s.url("/p/vinculum/Q-3/scope")).header(COOKIE, &cookie).header(ORIGIN, &s.base).form(&[("scope", m9.to_string())]).send().await.unwrap();
    assert!(res.status().is_redirection());
    let res = s.http.post(s.url("/p/vinculum/Q-3/answer")).header(COOKIE, &cookie).header(ORIGIN, &s.base).form(&[("words", "yes, everywhere")]).send().await.unwrap();
    assert!(res.status().is_redirection());
    // News comes in the next write call's reply, once; again shows it again.
    let (ok, r) = call("add_note", json!({"name": "n", "category": "log", "text": "x"})).await;
    assert!(ok, "{r}");
    let news = r["news"].to_string();
    assert!(news.contains("Owner answered Q-3") && news.contains("(M9)"), "{r}");
    let (_, again) = call("since_last_check", json!({})).await;
    assert_eq!(again["events"].as_array().unwrap().len(), 0, "{again}");
    let (_, again) = call("since_last_check", json!({"again": "1h"})).await;
    assert!(again.to_string().contains("Owner answered Q-3"), "{again}");
    let res = s.http.get(s.url("/api/news?again=60")).bearer_auth(&token).send().await.unwrap();
    assert!(res.text().await.unwrap().contains("Owner answered Q-3"));
    // Decisions: several re-scoped at once, promoted, replaced.
    let (ok, d) = call("record_decision", json!({"title": "New rule", "owner_words": "new", "source": "owner", "scope": "M8"})).await;
    assert!(ok, "{d}");
    let new_id = d["id"].as_str().unwrap().to_owned();
    let (ok, r) = call("update_decision", json!({"ids": ["D-1", new_id.clone()], "scope": "M9", "reason": "about M9"})).await;
    assert!(ok && r["decisions"].as_array().unwrap().len() == 2, "{r}");
    let (ok, r) = call("update_decision", json!({"id": "D-1", "replaced_by": new_id.clone(), "reason": "the owner changed it"})).await;
    assert!(ok && r["state"] == "replaced", "{r}");
    let (ok, r) = call("update_decision", json!({"id": new_id.clone(), "scope": "project", "reason": "holds everywhere"})).await;
    assert!(ok && r["scope"] == "project", "{r}");
    // Default reads: project-wide and open milestones' decisions; closed and replaced need a flag.
    let (ok, d) = call("record_decision", json!({"title": "M8 thing", "owner_words": "m8", "source": "owner", "scope": "M8"})).await;
    assert!(ok, "{d}");
    assert!(call("update_milestone", json!({"milestone": "M8", "state": "closed"})).await.0);
    let (_, found) = call("find_records", json!({"kind": "decision"})).await;
    let ids: Vec<String> = found["records"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_owned()).collect();
    let m8_thing = d["id"].as_str().unwrap().to_owned();
    assert!(!ids.contains(&"vinculum/D-1".to_owned()) && !ids.contains(&m8_thing), "{found}");
    let (_, found) = call("find_records", json!({"kind": "decision", "include_closed": true})).await;
    assert_eq!(found["count"], ids.len() + 2, "{found}");
    // Pages: count is the page, total all of them; compact leaves the quotes out.
    let (_, page) = call("find_records", json!({"kind": "decision", "include_closed": true, "limit": 1, "offset": 1, "compact": true})).await;
    assert!(page["count"] == 1 && page["total"] == ids.len() + 2 && page["more"] == true, "{page}");
    assert!(page["records"][0].get("owner_words").is_none() && page["records"][0]["scope"].is_string(), "{page}");
    let tab = s.http.get(s.url("/p/vinculum?tab=decisions")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(tab.contains("1 of closed milestones") && tab.contains("1 replaced by newer ones"), "{tab}");
    // hello lists project-wide decisions since the last session.
    let (_, h) = call("hello", hello).await;
    assert!(h["new_project_decisions"].to_string().contains("New rule"), "{h}");
}

/// `flow hello` for a SessionStart hook: the hello tool's reply over the API, news marked
/// read, the last session details kept when left out; only for a project's agent.
#[tokio::test]
async fn hello_over_the_api_for_a_session_start_hook() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let call = |name: &'static str, args: Value| {
        let (s, token, sess) = (&s, token.clone(), sess.clone());
        async move { mcp_tool(s, &token, &sess, name, args).await }
    };
    assert!(call("hello", json!({"session": "Vinculum", "machine": "giosuel.ch", "model": "Opus", "effort": "high"})).await.0);
    assert!(call("edit_project_doc", json!({"section": "How to work on it", "text": "- DCO sign-off"})).await.0);
    assert!(call("add_milestone", json!({"name": "M1", "title": "Done one", "description": "long text", "state": "closed"})).await.0);
    assert!(call("add_milestone", json!({"name": "M2", "title": "Now", "description": "what M2 is", "state": "active"})).await.0);
    let (ok, q) = call("ask_question", json!({"title": "Which?"})).await;
    assert!(ok, "{q}");
    let cookie = s.owner_cookie().await;
    let res = s.http.post(s.url("/p/vinculum/Q-1/answer")).header(COOKIE, &cookie).header(ORIGIN, &s.base).form(&[("words", "this one")]).send().await.unwrap();
    assert!(res.status().is_redirection());

    let res = s.http.post(s.url("/api/hello")).bearer_auth(&token).json(&json!({"source": "compact", "model": "Opus 5.5"})).send().await.unwrap();
    assert!(res.status().is_success(), "{}", res.status());
    let r: Value = res.json().await.unwrap();
    assert!(r["doc"]["text"].as_str().unwrap().contains("DCO sign-off"), "{r}");
    assert!(r["news"].to_string().contains("Owner answered Q-1"), "{r}");
    let text = crate::cli::hello_text(&r, "compact", crate::cli::HelloPart::All);
    assert!(text.starts_with("AgentFlow hello after your context was summarized"), "{text}");
    assert!(text.contains("DCO sign-off") && text.contains("Owner answered Q-1"), "{text}");
    assert!(text.contains("- M1 Done one [closed]") && !text.contains("long text") && text.contains("what M2 is"), "{text}");
    // The news was marked read; the session's name stayed, the model changed.
    let res = s.http.get(s.url("/api/news")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.json::<Value>().await.unwrap()["events"].as_array().unwrap().len(), 0);
    assert_eq!(r["project"]["agent"]["session"], "Vinculum", "{r}");
    let model: String = s.db.call(|c| Ok(crate::projects::require(c, "vinculum")?.agent.model)).await.unwrap();
    assert_eq!(model, "Opus 5.5");
    // The two parts for hooks: the document alone, then the rest, each under the hook limit.
    let res = s.http.post(s.url("/api/hello")).bearer_auth(&token).json(&json!({"part": "doc"})).send().await.unwrap();
    let d: Value = res.json().await.unwrap();
    assert!(d.get("milestones").is_none() && d["doc"]["text"].as_str().unwrap().contains("DCO"), "{d}");
    let doc = crate::cli::hello_text(&d, "startup", crate::cli::HelloPart::Doc);
    assert!(doc.contains("DCO sign-off") && !doc.contains("=== Milestones"), "{doc}");
    let mut big = r.clone();
    big["doc"]["text"] = "- a rule\n".repeat(3000).into();
    big["recent_events"] = (0..200).map(|i| json!({"at": "2026-11-12T10:00:00Z", "what": format!("event {i} {}", "x".repeat(80)), "by": "x"})).collect();
    let doc = crate::cli::hello_text(&big, "startup", crate::cli::HelloPart::Doc);
    assert!(doc.chars().count() <= crate::cli::HOOK_LIMIT && doc.contains("the document goes on"), "{}", doc.len());
    let rest = crate::cli::hello_text(&big, "startup", crate::cli::HelloPart::NoDoc);
    assert!(rest.chars().count() <= crate::cli::HOOK_LIMIT, "{}", rest.len());
    assert!(!rest.contains("a rule") && rest.contains("=== Where you are") && rest.contains("Owner answered Q-1") && rest.contains("what M2 is") && rest.contains("cut to fit"), "{rest}");
    // Not for the owner.
    let res = s.http.post(s.url("/api/hello")).header(COOKIE, &cookie).header(ORIGIN, &s.base).json(&json!({})).send().await.unwrap();
    assert!(!res.status().is_success());
}

/// Every tool's schema is written out in full: no $ref for a client to follow (Imaginarium's
/// didn't see update_milestone's review).
#[tokio::test]
async fn tool_schemas_have_no_refs() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let (_, r) = mcp_call(&s, &token, Some(&sess), json!({"jsonrpc": "2.0", "id": 9, "method": "tools/list", "params": {}})).await;
    let tools = r.unwrap()["result"]["tools"].as_array().unwrap().clone();
    assert!(tools.len() > 20);
    for t in &tools {
        let schema = t["inputSchema"].to_string();
        assert!(!schema.contains("$ref"), "{}: {schema}", t["name"]);
    }
    let um = tools.iter().find(|t| t["name"] == "update_milestone").unwrap();
    assert!(um["inputSchema"]["properties"]["review"]["properties"]["reports"].is_object(), "{um}");
}

/// "Set up on another machine" (the owner, 2026-11-13): the page makes a setup code, `flow
/// project setup` there trades it once for the project's token and says where the project lies.
#[tokio::test]
async fn a_setup_code_sets_a_project_up_on_a_machine() {
    let s = TestServer::start().await;
    let agent = s.token(TokenKind::Project, Some("vinculum")).await;
    let cookie = s.owner_cookie().await;
    let page = s.http.get(s.url("/p/vinculum")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("/p/vinculum/setup-code"), "{page}");
    let res = s.http.post(s.url("/p/vinculum/setup-code")).bearer_auth(&agent).send().await.unwrap();
    assert!(!res.status().is_success(), "not for an agent");
    let shown = s
        .http
        .post(s.url("/p/vinculum/setup-code"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(shown.contains("install.sh | sh") && shown.contains("flow project setup"), "{shown}");
    let start = shown.find("id=\"setup-code\">").unwrap() + "id=\"setup-code\">".len();
    let code: String = shown[start..].chars().take_while(|c| *c != '<').collect();
    assert_eq!(code.len(), 14, "{code}");
    assert!(!s.log_summaries().await.iter().any(|l| l.contains(&code)), "the code isn't logged");

    let redeem = |code: String| {
        let s = &s;
        async move { s.http.post(s.url("/api/setup/redeem")).json(&json!({"code": code, "machine": "giosuel@mothman"})).send().await.unwrap() }
    };
    let got: Value = redeem(code.to_lowercase()).await.json().await.unwrap();
    assert_eq!(got["project"], "vinculum");
    let token = got["token"].as_str().unwrap().to_owned();
    assert_eq!(redeem(code).await.status(), StatusCode::FORBIDDEN, "once");
    assert_eq!(redeem("AAAA-BBBB-CCCC".into()).await.status(), StatusCode::FORBIDDEN);

    let res = s
        .http
        .post(s.url("/api/setup/done"))
        .bearer_auth(&token)
        .json(&json!({"folder": "/home/giosuel/vinculum", "machine": "giosuel@mothman"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = s.http.post(s.url("/api/setup/done")).bearer_auth(&token).json(&json!({"folder": "relative", "machine": "m"})).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let page = s.http.get(s.url("/p/vinculum")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("/home/giosuel/vinculum") && page.contains("mothman") && page.contains("Set up on another machine"), "{page}");
}

/// Add project: the commands for `flow project setup`, or a project created here with a setup code.
#[tokio::test]
async fn add_project_hands_out_the_commands_or_a_setup_code() {
    let s = TestServer::start().await;
    let cookie = s.owner_cookie().await;
    let page = s.http.get(s.url("/projects/new")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("install.sh | sh") && page.contains("flow project setup"), "{page}");
    let shown = s
        .http
        .post(s.url("/projects/new"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("name", "Speculum Tool"), ("description", "Mirror")])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(shown.contains("id=\"setup-code\"") && shown.contains("cd ~/speculum-tool"), "{shown}");
    let p = s.db.call(|c| crate::projects::require(c, "speculum-tool")).await.unwrap();
    assert!(p.checkout.is_empty(), "no folder until it's set up");
    let tokens: i64 = s.db.call(move |c| Ok(c.query_row("SELECT count(*) FROM tokens WHERE project_id = ?1", [p.id], |r| r.get(0))?)).await.unwrap();
    assert_eq!(tokens, 0, "the setup code brings the token");
}

/// `flow sync` (the owner, 2026-11-13): a project's conversations reach AgentFlow from wherever
/// it lies, counted for that project, never twice.
#[tokio::test]
async fn flow_sync_sends_each_call_once_for_its_project() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let other = s.token(TokenKind::Project, Some("imaginarium")).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("s1.jsonl");
    let line = |id: &str, at: &str| {
        format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{at}\",\"sessionId\":\"s1\",\"cwd\":\"/home/giosuel/vinculum\",\"message\":{{\"id\":\"{id}\",\"model\":\"claude-opus-5-5\",\"role\":\"assistant\",\"usage\":{{\"input_tokens\":5,\"output_tokens\":10,\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":1000}}}}}}\n"
        )
    };
    std::fs::write(&file, line("m1", "2026-11-13T01:00:00Z") + &line("m2", "2026-11-13T01:01:00Z")).unwrap();
    let send = |tok: String, f: std::path::PathBuf, base: String| {
        tokio::task::spawn_blocking(move || crate::sync::send(&crate::cli::Client::with_token(&base, &tok), &[f]).unwrap())
    };
    assert_eq!(send(token.clone(), file.clone(), s.base.clone()).await.unwrap(), 2);
    assert_eq!(send(token.clone(), file.clone(), s.base.clone()).await.unwrap(), 0, "nothing twice");
    // Another project's token keeps its own offsets: its upload counts nothing new either.
    assert_eq!(send(other, file.clone(), s.base.clone()).await.unwrap(), 0);
    let mut f = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
    std::io::Write::write_all(&mut f, line("m3", "2026-11-13T01:02:00Z").as_bytes()).unwrap();
    assert_eq!(send(token.clone(), file.clone(), s.base.clone()).await.unwrap(), 1);
    let rows: Vec<(String, Option<String>)> = s
        .db
        .call(|c| {
            let mut st = c.prepare("SELECT u.message_id, p.slug FROM usage u LEFT JOIN projects p ON p.id = u.project_id ORDER BY 1")?;
            let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|(_, p)| p.as_deref() == Some("vinculum")), "{rows:?}");
    // Only projects' tokens sync.
    let device = s.token(TokenKind::Device, None).await;
    let res = s.http.post(s.url("/api/sync/state")).bearer_auth(&device).json(&json!({"file": "m:/x"})).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}
