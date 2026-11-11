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
    pub state: AppState,
    _data: tempfile::TempDir,
}

impl TestServer {
    pub async fn start() -> Self {
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
            ..Config::default()
        };
        let github = std::sync::Arc::new(crate::github::fake::FakeGitHub::default());
        let state = AppState::new(db.clone(), config, crate::push::Vapid::random(), github.clone());
        let app = router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        Self { base: format!("http://{addr}"), db, http, github, state, _data: data }
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
        .form(&[("option", "2"), ("words", "B, smaller"), ("next", "//evil.example")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()[LOCATION], "/?answered=vinculum/Q-1", "no open redirects; back to the Dashboard");

    let (_, q) = mcp_tool(&s, &token, &sess, "get_record", json!({"id": "Q-1"})).await;
    assert_eq!(
        (q["state"].as_str(), q["data"]["answer"].as_str(), q["data"]["answer_option"].as_u64()),
        (Some("answered"), Some("B, smaller"), Some(2))
    );
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
async fn the_dashboard_features_a_question_and_answers_in_one_tap() {
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
    // The own-words box is in a form without option buttons: Enter there must not also pick one.
    let words = html.find(r#"name="words""#).unwrap();
    let form_start = html[..words].rfind("<form").unwrap();
    let form_end = words + html[words..].find("</form>").unwrap();
    assert!(!html[form_start..form_end].contains(r#"name="option""#), "Enter in the words box would send an option");
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
    assert_eq!(res["run"], "agentflow run imaginarium 1");

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

    // It waits for the owner, who gives feedback on its page.
    let cookie = s.owner_cookie().await;
    let page = s.http.get(s.url("/")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Builds to try · 1"));
    let page =
        s.http.get(s.url("/p/imaginarium/B-1")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("How did it go?") && page.contains("agentflow run imaginarium 1"));
    let res = s
        .http
        .post(s.url("/p/imaginarium/B-1/feedback"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("v0", "works"), ("v1", "problem"), ("n1", "the border is cut off"), ("note", "")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()[LOCATION], "/p/imaginarium/B-1?sent=1");

    let sess = mcp_session(&s, &token).await;
    let (_, builds) = mcp_tool(&s, &token, &sess, "builds", json!({})).await;
    assert_eq!(builds["builds"][0]["state"], "tried");
    let fb = builds["builds"][0]["feedback"].as_array().unwrap();
    assert!(fb.iter().any(|f| f["verdict"] == "problem" && f["owner_words"] == "the border is cut off"), "{builds}");
    let (_, news) = mcp_tool(&s, &token, &sess, "since_last_check", json!({})).await;
    assert!(news.to_string().contains("1 works, 1 problem"), "{news}");
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
async fn the_owner_writes_into_a_conversation() {
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
    let res = s
        .http
        .post(s.url(&format!("/conversations/{thread}/reply")))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("body", "Use the .a8 suffix.")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()[LOCATION], format!("/conversations?t={thread}&sent=1"));
    let (_, news) = mcp_tool(&s, &v, &vs, "since_last_check", json!({})).await;
    assert!(news.to_string().contains("Use the .a8 suffix."), "{news}");
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
    assert_eq!(res.headers()[LOCATION], "/guides?error=pull");
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
