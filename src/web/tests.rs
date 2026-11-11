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
}

impl TestServer {
    pub async fn start() -> Self {
        let db = Db::open_memory();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = Config { listen: addr, public_url: format!("http://{addr}"), ..Config::default() };
        let app = router(AppState::new(db.clone(), config, crate::push::Vapid::random()))
            .into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        Self { base: format!("http://{addr}"), db, http }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub async fn token(&self, kind: TokenKind, project: Option<&str>) -> String {
        let project = project.map(str::to_owned);
        self.db
            .call(move |c| {
                if let Some(p) = &project {
                    c.execute("INSERT OR IGNORE INTO projects (slug, name, created_at) VALUES (?1, ?1, 0)", [p])?;
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
    let res = s.http.post(s.url("/settings/devices")).bearer_auth(&admin).form(&[("name", "x")]).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    let cookie = s.owner_cookie().await;
    let page = s.http.get(s.url("/settings")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    // A device token is shown once, right after it's made, and works.
    let res = s
        .http
        .post(s.url("/settings/devices"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("name", "HEADQUARTERS viber-run")])
        .send()
        .await
        .unwrap();
    let html = res.text().await.unwrap();
    let start = html.find("af_dev_").expect("token shown");
    let token: String =
        html[start..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    let me: Value = s.http.get(s.url("/api/whoami")).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    assert_eq!(me["kind"], "device");
    assert!(
        !s.http
            .get(s.url("/settings"))
            .header(COOKIE, &cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
            .contains(&token)
    );
    // …and never lands in the log.
    assert!(!s.log_summaries().await.iter().any(|l| l.contains(&token)));
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
    let (_, found) = call("find_records", json!({"project": "all", "text": "vertical"})).await;
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
