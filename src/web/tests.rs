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
        call("add_release", json!({"tag": "v0.0.5-m4", "title": "M4: Keybinds", "waits_for": ["PR-48", "PR-49"]}))
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

    // A folder with an app and a data file, packed like `agentflow publish` does.
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
    assert_eq!(res["run"], "flow-run imaginarium 1");

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
    assert!(!page.contains("How did it go?") && page.contains("flow-run imaginarium 1"));
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
}

#[tokio::test]
async fn agents_and_the_consultant_talk_through_agentflow() {
    let s = TestServer::start().await;
    let v = s.token(TokenKind::Project, Some("vinculum")).await;
    let a = s.token(TokenKind::Project, Some("antimony")).await;
    let consultant = s.token(TokenKind::Admin, None).await;
    let (vs, as_, cs) = (mcp_session(&s, &v).await, mcp_session(&s, &a).await, mcp_session(&s, &consultant).await);

    let (ok, r) = mcp_tool(&s, &v, &vs, "send_message", json!({"to": ["antimony"], "subject": "Port 21300?", "body": "Can I use 21300 for a demo?", "kind": "resource"})).await;
    assert!(ok, "{r}");
    let thread = r["conversation"].as_i64().unwrap();

    // Antimony's next reply of any tool says something is waiting; since_last_check has it.
    let (_, info) = mcp_tool(&s, &a, &as_, "project_info", json!({})).await;
    assert!(info["unread"].as_str().unwrap_or("").starts_with("1 new"), "{info}");
    let (_, news) = mcp_tool(&s, &a, &as_, "since_last_check", json!({})).await;
    assert!(news.to_string().contains("Can I use 21300"), "{news}");
    mcp_tool(
        &s,
        &a,
        &as_,
        "send_message",
        json!({"conversation": thread, "body": "Yes, it's free.", "kind": "answer"}),
    )
    .await;
    let (_, conv) = mcp_tool(&s, &v, &vs, "conversation", json!({"conversation": thread})).await;
    assert_eq!(conv["messages"].as_array().unwrap().len(), 2);

    // The consultant waits for its messages; a question to it ends the wait.
    let waiter = {
        let (http, url, t) = (s.http.clone(), s.url("/api/wait?timeout=20"), consultant.clone());
        tokio::spawn(async move { http.get(url).bearer_auth(t).send().await.unwrap().json::<Value>().await.unwrap() })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    mcp_tool(&s, &v, &vs, "send_message", json!({"to": ["consultant"], "subject": "Tags for A milestones", "body": "How do A milestones get tagged?", "kind": "question"})).await;
    let got = tokio::time::timeout(std::time::Duration::from_secs(10), waiter).await.unwrap().unwrap();
    assert!(got.to_string().contains("How do A milestones"), "{got}");
    // It stays unread until the consultant says it handled it.
    let (_, inbox) = mcp_tool(&s, &consultant, &cs, "inbox", json!({})).await;
    let list = inbox["messages"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{inbox}");
    let (_, inbox) = mcp_tool(&s, &consultant, &cs, "inbox", json!({"read_up_to": list[0]["id"]})).await;
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 0, "marked read");
    // An agent can't read a conversation it isn't in, nor the consultant's inbox.
    let (ok, _) = mcp_tool(&s, &a, &as_, "conversation", json!({"conversation": thread + 1})).await;
    assert!(!ok);
    let (ok, _) = mcp_tool(&s, &a, &as_, "inbox", json!({})).await;
    assert!(!ok);
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

#[tokio::test]
async fn the_owner_reads_conversations_but_doesnt_write() {
    let s = TestServer::start().await;
    let v = s.token(TokenKind::Project, Some("vinculum")).await;
    let vs = mcp_session(&s, &v).await;
    let (_, r) = mcp_tool(
        &s,
        &v,
        &vs,
        "send_message",
        json!({"to": ["consultant"], "subject": "Tags?", "body": "How?", "kind": "question"}),
    )
    .await;
    let thread = r["conversation"].as_i64().unwrap();
    let cookie = s.owner_cookie().await;
    let page = s.http.get(s.url("/conversations")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Tags?") && page.contains("vinculum → Consultant"), "names, not slugs");
    assert!(!page.contains("<textarea"), "no reply box");
    let res = s
        .http
        .post(s.url(&format!("/conversations/{thread}/reply")))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("body", "Use the .a8 suffix.")])
        .send()
        .await
        .unwrap();
    assert!(res.status().is_client_error(), "{}", res.status());
    // Tokens can't use the page.
    assert_eq!(
        s.http.get(s.url("/conversations")).bearer_auth(&v).send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
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
    assert_eq!(p.stack, ["Rust", "Slint", "zbus"]);
    assert_eq!(p.pm_rules, ["One rule", "Another"]);
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

/// The commits on top of `base` in `work`, as `agentflow contribute` sends them.
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
async fn devices_download_the_program_for_flow_run() {
    let s = TestServer::start().await;
    let res = s.http.get(s.url("/download/agentflow")).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let device = s.token(TokenKind::Device, None).await;
    let res = s.http.get(s.url("/download/agentflow")).bearer_auth(&device).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.bytes().await.unwrap();
    assert_eq!(&bytes[..4], b"\x7fELF", "the program itself");
}
