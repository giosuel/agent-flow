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
        // A handbook of the test's own, brought in from an old checkout like at a first start.
        let handbook = data.path().join("ai-handbook");
        std::fs::create_dir(&handbook).unwrap();
        std::fs::write(handbook.join("MACHINE.md"), "# MACHINE\n\nThe server.\n\n## Ports\n\nUse 10.0.8.1.\n").unwrap();
        {
            let (h, d) = (handbook.clone(), data.path().to_owned());
            db.call(move |c| {
                crate::handbook::import(c, &h)?;
                crate::handbook::mirror(c, &d)
            })
            .await
            .unwrap();
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
    /// The admin, with a profile, made the first time it's asked for.
    pub async fn admin(&self) -> i64 {
        self.db
            .call(|c| {
                if let Some(a) = crate::people::admin(c)? {
                    return Ok(a.id);
                }
                let id = crate::people::create(c, true, None, webauthn_rs::prelude::Uuid::new_v4())?;
                c.execute(
                    "UPDATE people SET username = 'giosuel', email = 'me@giosuel.ch', icon = 'owl' WHERE id = ?1",
                    [id],
                )?;
                Ok(id)
            })
            .await
            .unwrap()
    }

    /// Signs the admin in through a one-time link and returns the session cookie.
    pub async fn owner_cookie(&self) -> String {
        let admin = self.admin().await;
        self.cookie_for(admin).await
    }

    /// Signs a person in through a one-time link and returns the session cookie.
    pub async fn cookie_for(&self, person: i64) -> String {
        let link = self
            .db
            .call(move |c| auth::create_login_link(c, auth::purpose::SIGNIN, Some(person), None, auth::LINK_MINUTES))
            .await
            .unwrap();
        let res =
            self.http.post(self.url(&format!("/login/link/{link}"))).header(ORIGIN, &self.base).send().await.unwrap();
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
    let admin = s.admin().await;
    let link =
        s.db.call(move |c| auth::create_login_link(c, auth::purpose::SIGNIN, Some(admin), None, auth::LINK_MINUTES))
            .await
            .unwrap();
    // Opening the link only shows a confirmation; a link preview can't use it up.
    let page = s.http.get(s.url(&format!("/login/link/{link}"))).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Sign in on this device?"));

    let res = s.http.post(s.url(&format!("/login/link/{link}"))).header(ORIGIN, &s.base).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let set = res.headers()[SET_COOKIE].to_str().unwrap().to_owned();
    assert!(set.contains("HttpOnly") && set.contains("SameSite=Lax"));
    let cookie = set.split(';').next().unwrap().to_owned();

    let page = s.http.get(s.url("/")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.text().await.unwrap().contains("Dashboard"));

    let again = s.http.post(s.url(&format!("/login/link/{link}"))).header(ORIGIN, &s.base).send().await.unwrap();
    assert_eq!(again.status(), StatusCode::GONE);
}

/// The way in (the owner, 2026-11-25: D-273, D-274): `sudo flow server setup` makes the admin,
/// `invite` someone new, `add-device` a link for a new device; setup again makes the admin anew.
/// Each link page only shows itself; using the link signs in (here with the link itself: the test
/// server's plain http can't do passkeys).
#[tokio::test]
async fn people_get_in_with_setup_invite_and_device_links() {
    let s = TestServer::start().await;
    let ask = |req: serde_json::Value| {
        let state = s.state.clone();
        async move { crate::web::admin_socket::test_link(&state, req).await }
    };
    let secret = |v: &serde_json::Value| v["url"].as_str().unwrap().rsplit('/').next().unwrap().to_owned();
    let cookie_of =
        |res: &reqwest::Response| res.headers()[SET_COOKIE].to_str().unwrap().split(';').next().unwrap().to_owned();
    let whoami = |cookie: String| {
        let s = &s;
        async move {
            // The welcome page works before someone picked their name; nothing else does.
            let res = s.http.get(s.url("/welcome")).header(COOKIE, &cookie).send().await.unwrap();
            res.status()
        }
    };
    assert!(ask(json!({"request": "invite"})).await.is_err(), "nobody to invite from yet");
    assert!(ask(json!({"request": "add_passkey"})).await.unwrap_err().to_string().contains("add-device"));

    // Setup makes the admin.
    let v = ask(json!({"request": "setup"})).await.unwrap();
    assert!(v["url"].as_str().unwrap().contains("/setup/af_lnk_") && v["again"] == false, "{v}");
    let setup = secret(&v);
    let page = s.http.get(s.url(&format!("/setup/{setup}"))).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Set up AgentFlow") && page.contains("You'll be this AgentFlow's admin"), "{page}");
    assert!(page.contains("Sign in with this link"), "no passkeys on plain http: {page}");
    // The link is for /setup/ only: the wrong page refuses it and leaves it unused.
    let wrong = s.http.post(s.url(&format!("/device/{setup}"))).header(ORIGIN, &s.base).send().await.unwrap();
    assert_eq!(wrong.status(), StatusCode::GONE);
    let res = s.http.post(s.url(&format!("/setup/{setup}"))).header(ORIGIN, &s.base).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let admin_cookie = cookie_of(&res);
    assert_eq!(whoami(admin_cookie.clone()).await, StatusCode::OK);
    let again = s.http.post(s.url(&format!("/setup/{setup}"))).header(ORIGIN, &s.base).send().await.unwrap();
    assert_eq!(again.status(), StatusCode::GONE, "it works once");
    let admin = s.db.call(|c| crate::people::admin(c)).await.unwrap().unwrap();
    assert!(admin.admin && admin.username.is_none());

    // An invite makes someone new, who isn't the admin.
    let v = ask(json!({"request": "invite"})).await.unwrap();
    assert!(v["url"].as_str().unwrap().contains("/invite/"), "{v}");
    let invite = secret(&v);
    let page = s.http.get(s.url(&format!("/invite/{invite}"))).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("You're invited") && page.contains("This invite works once, until"), "{page}");
    let res = s.http.post(s.url(&format!("/invite/{invite}"))).header(ORIGIN, &s.base).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()["location"], "/welcome?next=/settings?add_passkey=1", "who are you first");
    let mira_cookie = cookie_of(&res);
    let everyone = s.db.call(|c| crate::people::list(c)).await.unwrap();
    assert_eq!(everyone.len(), 2);
    let mira = everyone.iter().find(|p| !p.admin).unwrap().clone();
    assert_eq!(mira.invited_by, Some(admin.id));

    // Only the admin's invites work: one made by someone else is refused and stays unused.
    let forged = s
        .db
        .call(move |c| auth::create_login_link(c, auth::purpose::INVITE, None, Some(mira.id), auth::INVITE_MINUTES))
        .await
        .unwrap();
    let res = s.http.post(s.url(&format!("/invite/{forged}"))).header(ORIGIN, &s.base).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(s.db.call(|c| crate::people::list(c)).await.unwrap().len(), 2, "nobody new");

    // A device link for someone named signs in as them; an unknown name is refused.
    s.db.call(move |c| Ok(c.execute("UPDATE people SET username = 'mira' WHERE id = ?1", [mira.id])?)).await.unwrap();
    assert!(ask(json!({"request": "add_device", "username": "nobody"})).await.is_err());
    let v = ask(json!({"request": "add_device", "username": "Mira"})).await.unwrap();
    assert_eq!(v["person"], "mira");
    let device = secret(&v);
    let page = s.http.get(s.url(&format!("/device/{device}"))).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Add this device") && page.contains("<strong>mira</strong>"), "{page}");

    // People can't touch each other's browsers.
    let admin_sessions: Vec<i64> =
        s.db.call(move |c| {
            let mut st = c.prepare("SELECT id FROM web_sessions WHERE person_id = ?1")?;
            Ok(st.query_map([admin.id], |r| r.get(0))?.collect::<Result<_, _>>()?)
        })
        .await
        .unwrap();
    let res = s
        .http
        .post(s.url(&format!("/settings/sessions/{}/end", admin_sessions[0])))
        .header(COOKIE, &mira_cookie)
        .header(ORIGIN, &s.base)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND, "not mira's browser");
    assert_eq!(whoami(admin_cookie.clone()).await, StatusCode::OK);

    // Setup again needs the admin typed back, and signs out the admin's browsers when it's used.
    assert!(ask(json!({"request": "setup", "confirm": "mira"})).await.is_err());
    let v = ask(json!({"request": "setup", "confirm": "yes"})).await.unwrap();
    assert_eq!(v["again"], true);
    let page = s.http.get(s.url(&format!("/setup/{}", secret(&v)))).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("This makes a new passkey for <strong>the admin</strong>"), "{page}");
    let res = s.http.post(s.url(&format!("/setup/{}", secret(&v)))).header(ORIGIN, &s.base).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_ne!(whoami(admin_cookie).await, StatusCode::OK, "the admin's old browser is signed out");
    assert_eq!(whoami(cookie_of(&res)).await, StatusCode::OK);
    assert_eq!(whoami(mira_cookie).await, StatusCode::OK, "the others stay");
    assert_eq!(s.db.call(|c| crate::people::list(c)).await.unwrap().len(), 2, "still the same admin");

    // An expired link shows that it expired.
    let old =
        s.db.call(move |c| auth::create_login_link(c, auth::purpose::DEVICE, Some(mira.id), None, auth::LINK_MINUTES))
            .await
            .unwrap();
    s.db.call(|c| Ok(c.execute("UPDATE login_links SET expires_at = 0 WHERE used_at IS NULL", [])?)).await.unwrap();
    let res = s.http.get(s.url(&format!("/device/{old}"))).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::GONE);
    assert!(res.text().await.unwrap().contains("doesn't work any more"));
    let log = s.log_summaries().await;
    assert!(log.iter().any(|l| l.contains("An invite was made on the server, as root")), "{log:?}");
    assert!(log.iter().any(|l| l.contains("invite link refused")), "{log:?}");
}

/// The first sign-in's step 2 (D-270): someone new picks a username, an email and an icon before
/// anything else opens.
#[tokio::test]
async fn someone_new_says_who_they_are_first() {
    let s = TestServer::start().await;
    let admin = s.admin().await;
    let mira =
        s.db.call(move |c| crate::people::create(c, false, Some(admin), webauthn_rs::prelude::Uuid::new_v4()))
            .await
            .unwrap();
    let cookie = s.cookie_for(mira).await;
    let res = s
        .http
        .get(s.url("/p/vinculum?tab=notes"))
        .header(COOKIE, &cookie)
        .header("accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()["location"], "/welcome?next=/p/vinculum");
    let page = owner_page(&s, &cookie, "/welcome?next=/p/vinculum").await;
    assert!(page.contains("Welcome! Who are you?") && page.contains(r#"name="next" value="/p/vinculum""#), "{page}");
    assert_eq!(page.matches(r#"name="icon""#).count(), 15, "the 15 icons");
    let post = |form: Vec<(&'static str, &'static str)>| {
        let (s, cookie) = (&s, cookie.clone());
        async move {
            s.http
                .post(s.url("/welcome"))
                .header(COOKIE, &cookie)
                .header(ORIGIN, &s.base)
                .form(&form)
                .send()
                .await
                .unwrap()
        }
    };
    let res = post(vec![("username", "giosuel"), ("email", "m@x.org"), ("icon", "fox"), ("next", "/p/vinculum")]).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "taken");
    assert!(res.text().await.unwrap().contains("already called"));
    let res = post(vec![("username", "Mira"), ("email", "m@x.org"), ("icon", "fox"), ("next", "//evil.example")]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers()["location"], "/", "only paths on this site");
    let p = s.db.call(move |c| crate::people::get(c, mira)).await.unwrap().unwrap();
    assert_eq!((p.username.as_deref(), p.email.as_str(), p.icon.as_str()), (Some("mira"), "m@x.org", "fox"));
    let res = s.http.get(s.url("/")).header(COOKIE, &cookie).header("accept", "text/html").send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "everything opens now");
}

/// You and the others (D-270, D-272, D-273, D-274): your icon at the top right with your own
/// settings; Settings › Profile, Passkeys' link for another device, People with the admin's
/// invites and sign-in links, and withdrawing a link.
#[tokio::test]
async fn settings_for_you_and_the_people() {
    let s = TestServer::start().await;
    let admin = s.admin().await;
    let cookie = s.owner_cookie().await;
    let page = owner_page(&s, &cookie, "/").await;
    let header = &page[page.find("<header").unwrap()..page.find("</header>").unwrap()];
    assert!(header.contains(r#"<details class="me-menu" data-menu>"#) && header.contains("me@giosuel.ch"), "{header}");
    assert!(header.contains("/static/avatars/owl.svg") && header.contains(">Sign out<"), "{header}");
    let foot = &page[page.find("<footer").unwrap()..];
    assert!(!foot.contains("Sign out"), "Sign out moved to the menu: {foot}");

    // Profile.
    let profile = owner_page(&s, &cookie, "/settings?s=profile").await;
    assert!(profile.contains(r#"value="giosuel""#) && profile.contains(r#"value="owl" checked"#), "{profile}");
    let post = |path: &'static str, form: Vec<(&'static str, &'static str)>, cookie: String| {
        let s = &s;
        async move {
            s.http.post(s.url(path)).header(COOKIE, &cookie).header(ORIGIN, &s.base).form(&form).send().await.unwrap()
        }
    };
    let res = post(
        "/settings/profile",
        vec![("username", "gio"), ("email", "me@giosuel.ch"), ("icon", "fox")],
        cookie.clone(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let me = s.db.call(move |c| crate::people::get(c, admin)).await.unwrap().unwrap();
    assert_eq!((me.username.as_deref(), me.icon.as_str()), (Some("gio"), "fox"));

    // A link for another device of your own, with its QR code.
    let res = post("/settings/passkeys/link", vec![], cookie.clone()).await;
    let text = res.text().await.unwrap();
    assert!(text.contains("/device/af_lnk_") && text.contains(r#"<svg class="qr""#), "{text}");

    // People: the admin invites.
    let people = owner_page(&s, &cookie, "/settings?s=people").await;
    assert!(people.contains("Invite someone") && people.contains("only you, the admin, can invite"), "{people}");
    let res = post("/settings/people/invite", vec![], cookie.clone()).await;
    let text = res.text().await.unwrap();
    let at = text.find("/invite/af_lnk_").expect("the invite link");
    let secret: String = text[at + 8..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    assert!(text.contains("An invite, not used yet"), "{text}");
    let id: i64 =
        s.db.call(|c| {
            Ok(c.query_row("SELECT rowid FROM login_links WHERE purpose = 'invite' AND used_at IS NULL", [], |r| {
                r.get(0)
            })?)
        })
        .await
        .unwrap();

    // Someone who isn't the admin sees the people, but can't invite or withdraw the admin's invite.
    let mira =
        s.db.call(move |c| {
            let m = crate::people::create(c, false, Some(admin), webauthn_rs::prelude::Uuid::new_v4())?;
            crate::people::set_profile(c, m, "mira", "m@x.org", "frog")?;
            Ok(m)
        })
        .await
        .unwrap();
    let mira_cookie = s.cookie_for(mira).await;
    let theirs = owner_page(&s, &mira_cookie, "/settings?s=people").await;
    assert!(theirs.contains("only the admin, gio, invites people") && !theirs.contains("Invite someone"), "{theirs}");
    assert_eq!(post("/settings/people/invite", vec![], mira_cookie.clone()).await.status(), StatusCode::FORBIDDEN);
    let withdraw = format!("/settings/links/{id}/withdraw");
    let w: &'static str = Box::leak(withdraw.into_boxed_str());
    assert_eq!(post(w, vec![], mira_cookie.clone()).await.status(), StatusCode::NOT_FOUND);
    // The admin's sign-in link for mira.
    let path: &'static str = Box::leak(format!("/settings/people/{mira}/link").into_boxed_str());
    let text = post(path, vec![], cookie.clone()).await.text().await.unwrap();
    assert!(text.contains("A sign-in link for mira") && text.contains("/device/af_lnk_"), "{text}");
    assert_eq!(post(path, vec![], mira_cookie).await.status(), StatusCode::FORBIDDEN, "only the admin");

    // Withdrawn, the invite stops working.
    assert_eq!(post(w, vec![], cookie.clone()).await.status(), StatusCode::SEE_OTHER);
    let res = s.http.get(s.url(&format!("/invite/{secret}"))).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::GONE);
}

/// What the security review of People found (2026-11-25), fixed: setup links bound to the state
/// they were made for, setting up again voiding the admin's links and devices, login CSRF on the
/// link pages, /login/link only for sign-in links, removing people, names that can't pass for
/// someone else, and `next` kept on this site.
#[tokio::test]
async fn the_review_of_people_holds() {
    let s = TestServer::start().await;
    let ask = |req: serde_json::Value| {
        let state = s.state.clone();
        async move { crate::web::admin_socket::test_link(&state, req).await.unwrap() }
    };
    let secret = |v: &serde_json::Value| v["url"].as_str().unwrap().rsplit('/').next().unwrap().to_owned();
    let post = |path: String, origin: &'static str| {
        let s = &s;
        async move {
            let origin = if origin.is_empty() { s.base.clone() } else { origin.to_owned() };
            s.http.post(s.url(&path)).header(ORIGIN, origin).send().await.unwrap()
        }
    };

    // Two setup links before there's an admin: the first used makes the admin, the other stops.
    let early = secret(&ask(json!({"request": "setup"})).await);
    let first = secret(&ask(json!({"request": "setup"})).await);
    assert_eq!(post(format!("/setup/{first}"), "").await.status(), StatusCode::SEE_OTHER);
    assert_eq!(post(format!("/setup/{early}"), "").await.status(), StatusCode::GONE, "voided");
    let admin = s.db.call(|c| crate::people::admin(c)).await.unwrap().unwrap().id;
    s.db.call(move |c| crate::people::set_profile(c, admin, "giosuel", "me@giosuel.ch", "owl")).await.unwrap();

    // A setup link made for the first setup doesn't reset the admin made since.
    let stale =
        s.db.call(|c| auth::create_login_link(c, auth::purpose::SETUP, None, None, auth::LINK_MINUTES)).await.unwrap();
    assert_eq!(post(format!("/setup/{stale}"), "").await.status(), StatusCode::BAD_REQUEST);

    // Setting up again voids the admin's own links and push devices.
    let mine = secret(&ask(json!({"request": "add_device"})).await);
    s.db.call(move |c| {
        Ok(c.execute(
            "INSERT INTO push_subscriptions (endpoint, p256dh, auth, device, created_at, person_id) VALUES ('https://web.push.apple.com/x', 'k', 'a', 'd', 0, ?1)",
            [admin],
        )?)
    })
    .await
    .unwrap();
    let again = secret(&ask(json!({"request": "setup", "confirm": "giosuel"})).await);
    assert_eq!(post(format!("/setup/{again}"), "").await.status(), StatusCode::SEE_OTHER);
    assert_eq!(post(format!("/device/{mine}"), "").await.status(), StatusCode::GONE, "the admin's device link went");
    let devices = s.db.call(move |c| crate::push::devices(c, admin)).await.unwrap();
    assert!(devices.is_empty(), "and their devices' notifications");

    // Another site can't post a link for a browser (login CSRF); ours can.
    let link = secret(&ask(json!({"request": "add_device"})).await);
    assert_eq!(post(format!("/device/{link}"), "https://evil.example").await.status(), StatusCode::FORBIDDEN);
    // /login/link takes only sign-in links: a device link is refused there and stays unused.
    assert_eq!(post(format!("/login/link/{link}"), "").await.status(), StatusCode::GONE);
    assert_eq!(post(format!("/device/{link}"), "").await.status(), StatusCode::SEE_OTHER);

    // The admin removes someone: their browser is signed out, they can't come back.
    let mira =
        s.db.call(move |c| {
            let m = crate::people::create(c, false, Some(admin), webauthn_rs::prelude::Uuid::new_v4())?;
            crate::people::set_profile(c, m, "mira", "m@x.org", "frog")?;
            Ok(m)
        })
        .await
        .unwrap();
    let mira_cookie = s.cookie_for(mira).await;
    let admin_cookie = s.cookie_for(admin).await;
    let res = s
        .http
        .post(s.url(&format!("/settings/people/{admin}/remove")))
        .header(COOKIE, &admin_cookie)
        .header(ORIGIN, &s.base)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST, "not the admin");
    let res = s
        .http
        .post(s.url(&format!("/settings/people/{mira}/remove")))
        .header(COOKIE, &admin_cookie)
        .header(ORIGIN, &s.base)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let res = s.http.get(s.url("/welcome")).header(COOKIE, &mira_cookie).send().await.unwrap();
    assert_ne!(res.status(), StatusCode::OK, "signed out");
    assert!(s.db.call(move |c| crate::people::get(c, mira)).await.unwrap().is_none());

    // Names that would pass for someone else.
    let (ok_stand_in, ok_project) =
        s.db.call(move |c| {
            let x = crate::people::create(c, false, Some(admin), webauthn_rs::prelude::Uuid::new_v4())?;
            Ok((
                crate::people::set_profile(c, x, "person-3", "x@x.org", "fox").is_err(),
                crate::projects::create(c, "giosuel", "Giosuel", "owner").is_err(),
            ))
        })
        .await
        .unwrap();
    assert!(ok_stand_in && ok_project);

    // `next` stays on this site: a tab can't turn it into another site.
    let res = s
        .http
        .post(s.url("/welcome"))
        .header(COOKIE, &admin_cookie)
        .header(ORIGIN, &s.base)
        .form(&[("username", "giosuel"), ("email", "me@giosuel.ch"), ("icon", "owl"), ("next", "/\t/evil.example")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()["location"], "/");
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
async fn the_bell_keeps_notifications_and_opening_it_reads_them() {
    let s = TestServer::start().await;
    let cookie = s.owner_cookie().await;
    s.db.call(|c| crate::notifications::store(c, "question", "Vinculum asks: one icon?", "", "/p/vinculum/Q-1", None))
        .await
        .unwrap();
    let get = |path: &'static str| {
        let (s, cookie) = (&s, cookie.clone());
        async move { s.http.get(s.url(path)).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap() }
    };
    assert!(get("/").await.contains("bell-badge"), "a badge while one is unread");
    let panel = get("/notifications/panel").await;
    assert!(panel.contains("Vinculum asks: one icon?") && panel.contains(">New<"), "{panel}");
    let res =
        s.http.post(s.url("/notifications/read")).header(COOKIE, &cookie).header(ORIGIN, &s.base).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(!get("/").await.contains("bell-badge"), "the badge is gone once it's opened");
    assert!(get("/notifications/panel").await.contains(">Earlier<"));
    let agent = s.token(TokenKind::Project, Some("vinculum")).await;
    let res = s.http.get(s.url("/notifications/panel")).bearer_auth(&agent).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "the owner's alone");
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
    // One section at a time; the list names them all.
    let text = page.text().await.unwrap();
    assert!(text.contains("?s=clients") && text.contains("?s=consultant"), "the list of sections");
    let page = s.http.get(s.url("/settings?s=devices")).header(COOKIE, &cookie).send().await.unwrap();
    assert!(page.text().await.unwrap().contains("/install.sh | sh"), "the install command is under Devices");
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
            s.http
                .post(s.url("/api/approvals/poll"))
                .json(&json!({"poll": p}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    assert_eq!(polled(poll.clone()).await["state"], "pending");

    // Only the owner's browser sees and decides it; the consultant's token can't.
    let admin = s.token(TokenKind::Admin, None).await;
    let res = s.http.get(s.url(&format!("/approve/{code}"))).bearer_auth(&admin).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = s
        .http
        .post(s.url(&format!("/approve/{code}")))
        .bearer_auth(&admin)
        .form(&[("decision", "approve")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let page = s
        .http
        .get(s.url(&format!("/approve/{code}")))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains(&code) && page.contains("viber@mothman") && page.contains("Approve"), "{page}");
    let settings = s.http.get(s.url("/settings")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(settings.contains("1 waiting"), "the Devices section says one waits");
    let settings =
        s.http.get(s.url("/settings?s=devices")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
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
    let res = s
        .http
        .post(s.url("/api/approvals"))
        .bearer_auth(&project)
        .json(&json!({"action": "backup", "machine": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res =
        s.http.post(s.url("/api/approvals")).json(&json!({"action": "backup", "machine": "x"})).send().await.unwrap();
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
    assert_eq!(
        s.http.get(s.url("/api/status")).bearer_auth(&token).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    // The install script is public and names this server.
    let script = s.http.get(s.url("/install.sh")).send().await.unwrap().text().await.unwrap();
    assert!(
        script.starts_with("#!/bin/sh")
            && script.contains(&format!("url='{}'", s.base))
            && script.contains("flow\" connect"),
        "{script}"
    );
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
    let (_, found) =
        call("find_records", json!({"project": "all", "kind": "decision", "text": "vertical", "include_closed": true}))
            .await;
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

/// The overview (the owner, 2026-11-17, D-198): open questions on top with one-click answers, the
/// milestone under way large, planned ones as rows, closed ones folded.
#[tokio::test]
async fn the_overview_puts_questions_first_and_folds_closed_milestones() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    for (name, state) in [("M1", "closed"), ("M2", "active"), ("M3", "planned")] {
        mcp_tool(
            &s,
            &token,
            &sess,
            "add_milestone",
            json!({"name": name, "title": format!("Work {name}"), "state": state}),
        )
        .await;
    }
    mcp_tool(
        &s,
        &token,
        &sess,
        "ask_question",
        json!({"title": "A or B?", "options": [{"title": "Take A"}, {"title": "Take B"}], "recommended": 2}),
    )
    .await;
    // Five answers: the card shows three, the recommended fifth among them (D-234).
    mcp_tool(
        &s,
        &token,
        &sess,
        "ask_question",
        json!({"title": "Which?", "options": [{"title": "Opt one"}, {"title": "Opt two"}, {"title": "Opt three"}, {"title": "Opt four"}, {"title": "Opt five"}], "recommended": 5}),
    )
    .await;
    let cookie = s.owner_cookie().await;
    let page = s.http.get(s.url("/p/vinculum")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(
        page.contains("Waiting on you") && page.contains("Take A") && page.contains(r#"class="choice rec""#),
        "{page}"
    );
    assert!(page.contains("data-qpage") && page.contains(r#"<div class="pager" hidden>"#), "{page}");
    // On the amber island the pager is warm, not the greys made for dark cards.
    assert!(page.contains(r#"class="ibtn warn pg-prev""#) && page.contains(r#"class="ibtn warn qp-prev""#), "{page}");
    assert!(!page.contains(r#"class="ibtn qp-"#) && !page.contains(r#"class="ibtn pg-"#), "{page}");
    assert!(page.contains("Opt one") && page.contains("Opt two") && page.contains("Opt five"), "{page}");
    assert!(
        !page.contains("Opt three") && !page.contains("Opt four") && page.contains("2 more, or your own"),
        "{page}"
    );
    assert!(page.contains(r#"name="next" value="/p/vinculum?answered=Q-1""#), "{page}");
    // The overview has the milestone list in its own order, the one under way open (D-226).
    assert!(page.contains("1 done · 1 under way · 1 planned") && page.contains("Open in Plan"), "{page}");
    let (m1, m2, m3) = (page.find("Work M1").unwrap(), page.find("Work M2").unwrap(), page.find("Work M3").unwrap());
    assert!(m1 < m2 && m2 < m3, "done, under way, planned");
    assert!(page.contains(r#"<details class="ml-done" data-ml-done>"#), "the done ones folded on the overview");
    assert!(!page.contains("Open questions"), "the old card at the bottom is gone");
    // The state icons say what they mean on hover.
    for tip in [r#"title="Done""#, r#"title="Under way""#, r#"title="Planned"#] {
        assert!(page.contains(tip), "{tip}: {page}");
    }
    // One click answers, and the overview says so.
    let res = s
        .http
        .post(s.url("/p/vinculum/Q-1/answer"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("option", "2"), ("next", "/p/vinculum?answered=Q-1")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()[LOCATION], "/p/vinculum?answered=Q-1");
    let page = s
        .http
        .get(s.url("/p/vinculum?answered=Q-1"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("Answered Q-1") && !page.contains(r#"id="qt-Q-1""#) && page.contains(r#"id="qt-Q-2""#),
        "{page}"
    );
    // Plan: the same list, the done ones listed (in its scroll box), the one under way picked.
    let plan = owner_page(&s, &cookie, "/p/vinculum/plan").await;
    assert!(plan.contains(r#"<details class="ml-done" open data-ml-done>"#), "{plan}");
    assert!(
        plan.contains(r#"class="ml-row now picked open" href="/p/vinculum/plan/M2" data-ms aria-current="true""#),
        "{plan}"
    );
    // A closed milestone picked stays in its place, lit; the path ends at Plan (D-225).
    let plan = owner_page(&s, &cookie, "/p/vinculum/plan/M1").await;
    assert!(plan.contains(r#"class="ml-row done picked open" href="/p/vinculum/plan/M1""#), "{plan}");
    assert!(plan.contains(r#"<span aria-current="page" title="Plan">Plan</span>"#), "the path ends at Plan: {plan}");
}

/// M13: a status line's sample makes the Usage page's plan card, and the chart counts cost.
#[tokio::test]
async fn the_usage_page_shows_the_plan_and_the_cost() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let cookie = s.owner_cookie().await;
    let get = |path: &str| {
        let req = s.http.get(s.url(path)).header(COOKIE, &cookie);
        async move { req.send().await.unwrap().text().await.unwrap() }
    };
    assert!(get("/usage").await.contains("flow project setup"), "no samples yet: how to get them");
    let soon = crate::util::now_ms() / 1000 + 3 * 3600;
    let res = s
        .http
        .post(s.url("/api/usage/plan"))
        .bearer_auth(&token)
        .json(&json!({"rate_limits": {"five_hour": {"used_percentage": 42, "resets_at": soon}, "seven_day": {"used_percentage": 81, "resets_at": soon + 86400}}, "machine": "viber"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = s
        .http
        .post(s.url("/api/usage/plan"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .json(&json!({"rate_limits": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "only a project's status line sends them");
    let page = get("/usage").await;
    // No subtitle saying what the page is; Reload is an icon on the right (the owner, 2026-11-23).
    assert!(page.contains(r#"<button class="ibtn" type="submit""#) && page.contains("newest call"), "{page}");
    assert!(!page.contains("Tokens and their cost at API prices"), "the subtitle is gone");
    assert!(
        page.contains("42%") && page.contains("81%") && page.contains(r#"<div class="meter" role="meter""#),
        "{page}"
    );
    assert!(page.contains(r#"class="warn""#), "the week is past 80%: {page}");
    assert!(page.contains("pct-marks"), "the strip under the bars: {page}");
    let page = get("/usage?metric=cost").await;
    assert!(page.contains("Cost per day") && page.contains(r#"aria-current="true">Cost<"#), "{page}");
    assert!(get("/usage?metric=total").await.contains("Total tokens per day"));
    // Agents ask too: flow usage's API and the usage tool.
    let v: serde_json::Value =
        s.http.get(s.url("/api/usage")).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["plan"]["five_hour"]["used"], 42.0, "{v}");
    assert!(v["plan"]["week"]["resets_at"].as_str().is_some(), "{v}");
    let sess = mcp_session(&s, &token).await;
    let (ok, v) = mcp_tool(&s, &token, &sess, "usage", json!({"all": true})).await;
    assert!(ok && v["plan"]["week"]["used"] == 81.0, "{v}");
    // The week was past 80% with its first sample: one push, in the bell.
    let mut usage_pushes = 0;
    for _ in 0..50 {
        let bell = s.db.call(|c| crate::notifications::list(c, 1, 10)).await.unwrap();
        usage_pushes = bell.iter().filter(|n| n.kind == "usage").count();
        if usage_pushes > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(usage_pushes, 1);
}

#[tokio::test]
async fn a_question_without_options_is_answered_in_a_text_box() {
    // The owner, 2026-11-25 (D-287): "sometimes a question has no answers, so a text box there
    // would be appropriate instead".
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    mcp_tool(&s, &token, &sess, "ask_question", json!({"title": "Which port?"})).await;
    let cookie = s.owner_cookie().await;
    let overview = owner_page(&s, &cookie, "/p/vinculum").await;
    assert!(overview.contains(r#"<textarea id="qw-Q-1" name="words" rows="4""#), "{overview}");
    assert!(!overview.contains("No options"), "{overview}");
    let tab = owner_page(&s, &cookie, "/p/vinculum?tab=questions").await;
    assert!(tab.contains(r#"<textarea id="dqw-Q-1" name="words""#), "{tab}");
    let page = owner_page(&s, &cookie, "/p/vinculum/Q-1").await;
    assert!(page.contains(r#"<textarea id="words" name="words""#), "{page}");
    // The words are the answer.
    let res = s
        .http
        .post(s.url("/p/vinculum/Q-1/answer"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("words", "8990"), ("next", "/p/vinculum?answered=Q-1")])
        .send()
        .await
        .unwrap();
    assert!(res.status().is_redirection() || res.status().is_success(), "{}", res.status());
    let q = owner_page(&s, &cookie, "/p/vinculum/Q-1").await;
    assert!(q.contains("8990") && !q.contains(r#"name="words""#), "{q}");
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
        "/p/vinculum/plan/none",
        "/p/vinculum/plan",
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
    let page = page.text().await.unwrap();
    assert!(page.contains("not both"));
    // Sent from the question's page (or its dialog), the answer goes back to the project, not the
    // Dashboard (09-9).
    assert!(page.contains(r#"name="next" value="/p/vinculum?answered=Q-1""#), "{page}");
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
    // With none waiting, the Questions tab says so at its top (D-249), above the decision it made.
    let tab = owner_page(&s, &cookie, "/p/vinculum?tab=questions").await;
    let (none, decided) = (tab.find("No open questions.").unwrap(), tab.find("A or B?").unwrap());
    assert!(none < decided && !tab.contains("Waiting on you"), "{tab}");
    // The tab is the page's title; the project's name is the overview's.
    assert!(tab.contains("<h1>Questions</h1>"), "{tab}");
    // The overview's title is Overview too (D-281); the project's name is in the switcher and the path.
    let overview = owner_page(&s, &cookie, "/p/vinculum").await;
    assert!(overview.contains("<h1>Overview</h1>") && overview.contains("<title>Overview · "), "{overview}");
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
    assert!(html.contains(r#"<b class="badge warn gbadge">2</b>"#));

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
    assert!(html.contains(r#"<b class="badge warn gbadge">1</b>"#));
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
    assert!(!call("update_pr", json!({"number": 48})).await.0, "no repo yet");
    call("update_project", json!({"repo": "giosuel/vinculum"})).await;
    s.github.set("giosuel/vinculum", pr(48, "main", "b48"));
    s.github.set("giosuel/vinculum", pr(49, "b48", "b49"));
    crate::poller::poll_once(&s.state).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let wd = dir.path().display().to_string();
    for n in [48, 49] {
        let (ok, r) = call("update_pr", json!({"number": n})).await;
        assert!(ok, "{r}");
        // Readiness is GitHub's draft flag (D-233): the old field is refused with the gh command.
        let (ok, r) = call("update_pr", json!({"number": n, "ready": false})).await;
        assert!(!ok && r.to_string().contains("gh pr ready --undo"), "{r}");
        let (ok, r) = call("add_button", json!({"record": format!("PR-{n}"), "label": "Merge", "kind": "merge", "command": format!("echo merged {n} > merged-{n}"), "workdir": wd})).await;
        assert!(ok, "{r}");
    }
    let (ok, r) = call(
        "add_release",
        json!({"tag": "v0.0.5-m4", "title": "M4: Keybinds", "waits_for": ["PR-48", "PR-49"], "tag_button": false}),
    )
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
    assert_eq!(html.matches(">Merge</button>").count(), 1, "#49 has no Merge of its own, only #48 does");
    // The overview has no merge panel any more, only the count on the Pull requests tab.
    let overview = s.http.get(s.url("/p/vinculum")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(!overview.contains("Merge order") && !overview.contains("Merge #48–#49"), "no merge panel");
    assert!(overview.contains(r#"<span class="only-phone">PRs</span> <span class="count">2</span>"#), "the count");
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
    assert_eq!(res.headers()[LOCATION], "/p/vinculum?tab=prs&merging=48-49");
    // It runs on its own, after the redirect.
    wait_for_records(&s, "vinculum", &[48, 49], "merged").await;
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

/// A page's HTML, as the owner sees it.
/// The desktop bar (D-232): in a project the switcher names it and its tabs carry their counts;
/// on a global page the switcher says All projects and that page's icon is a pill with its name.
#[tokio::test]
async fn the_bar_shows_the_project_its_tabs_and_the_global_pages() {
    // The project's tabs are hidden on the phone (app.css, outside the desktop query) and shown
    // again on the desktop: that rule has to say display, or the hiding one wins over the
    // library's .tabs (it did in PR 7, 2026-11-24: the bar lost its tabs).
    let css = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static/app.css")).unwrap();
    assert!(css.contains(".pswitch, .btabs, .btabs-more, .vsep { display: none; }"), "the phone hides them");
    let desk =
        css.lines().find(|l| l.trim_start().starts_with(".btabs { ")).expect("the desktop rule for the bar's tabs");
    assert!(desk.contains("display: flex"), "the desktop shows the bar's tabs: {desk}");
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
    s.token(TokenKind::Project, Some("lituus")).await;
    let cookie = s.owner_cookie().await;
    let page = owner_page(&s, &cookie, "/p/vinculum?tab=questions").await;
    let bar = &page[page.find("<header").unwrap()..page.find("</header>").unwrap()];
    // Switching projects keeps the page (the owner, 2026-11-25, D-280): from Vinculum's Questions to
    // Lituus's; from Plan to Plan; from Settings (a dialog) to the overview.
    assert!(bar.contains(r#"href="/p/lituus?tab=questions""#), "{bar}");
    for (from, to) in
        [("/p/vinculum/plan", "/p/lituus/plan"), ("/p/vinculum/settings", "/p/lituus"), ("/p/vinculum", "/p/lituus")]
    {
        let page = owner_page(&s, &cookie, from).await;
        let menu = &page[page.find("pswitch-menu").unwrap()..page.find("New project").unwrap()];
        assert!(menu.contains(&format!(r#"href="{to}">"#)), "{from}: {menu}");
    }
    assert!(bar.contains(r#"<summary class="pswitch-btn"><span>vinculum</span>"#), "{bar}");
    assert!(
        bar.contains(r#"href="/p/vinculum?tab=questions" aria-current="page""#),
        "the project is checked in the switcher"
    );
    // Questions and Decisions are one tab, Questions (Q-6, D-250): it counts the open question.
    assert!(bar.contains(
        r#"<a href="/p/vinculum?tab=questions" class="on" aria-current="page">Questions<span class="count warn">1</span></a>"#
    ));
    assert!(!bar.contains(">Decisions<"), "{bar}");
    assert!(bar.contains(r#"href="/p/vinculum/settings" data-dlg>Settings"#));
    assert!(bar.contains(r#"aria-label="Usage" data-name="Usage""#), "global pages are icons");

    let usage = owner_page(&s, &cookie, "/usage").await;
    let bar = &usage[usage.find("<header").unwrap()..usage.find("</header>").unwrap()];
    assert!(bar.contains("<span>All projects</span>"));
    assert!(!bar.contains("btabs"), "no project tabs on a global page");
    assert!(bar.contains(r#"<a href="/usage" class="on" aria-current="page">"#));
    assert!(bar.contains("<span>Usage</span>"), "the current page's icon carries its name");

    // The project last opened is kept in a cookie, and the bar outside the projects shows it,
    // drawn by the server (the owner, 2026-11-23: "lets do cookie").
    let set_cookie = |res: &reqwest::Response| {
        res.headers().get_all(SET_COOKIE).iter().map(|v| v.to_str().unwrap().to_owned()).collect::<Vec<_>>().join("\n")
    };
    let res = s.http.get(s.url("/p/vinculum?tab=prs")).header(COOKIE, &cookie).send().await.unwrap();
    let kept = set_cookie(&res);
    assert!(kept.contains("af_project=vinculum") && kept.contains("HttpOnly") && kept.contains("Path=/"), "{kept}");
    let with = format!("{cookie}; af_project=vinculum");
    let res = s.http.get(s.url("/p/vinculum")).header(COOKIE, &with).send().await.unwrap();
    assert!(set_cookie(&res).is_empty(), "already kept");
    let usage = owner_page(&s, &with, "/usage").await;
    let bar = &usage[usage.find("<header").unwrap()..usage.find("</header>").unwrap()];
    assert!(bar.contains(r#"<summary class="pswitch-btn"><span>vinculum</span>"#), "{bar}");
    assert!(bar.contains(r#"<a href="/p/vinculum">Overview</a>"#), "its tabs, none lit: {bar}");
    assert!(
        bar.contains(r#"href="/p/lituus">"#),
        "from a page outside the projects, the switcher goes to the overview"
    );
    assert!(bar.contains(r#"Questions<span class="count warn">1</span>"#), "with their counts");
    assert!(bar.contains(r#"<a href="/usage" class="on" aria-current="page">"#), "the page is still Usage");
    let unknown = owner_page(&s, &format!("{cookie}; af_project=nope"), "/usage").await;
    assert!(unknown.contains("<span>All projects</span>"), "a project that isn't there is ignored");
    // All projects forgets it.
    let res = s.http.get(s.url("/projects")).header(COOKIE, &with).send().await.unwrap();
    let gone = set_cookie(&res);
    assert!(gone.contains("af_project=;") && gone.contains("Max-Age=0"), "{gone}");
    let list = res.text().await.unwrap();
    assert!(list.contains("<span>All projects</span>"), "the projects list isn't one project's");
}

/// A release's tag pushed by hand, outside AgentFlow: the poller sees it on GitHub, marks the
/// release tagged, closes its milestone and withdraws the Push tag button, which would collide
/// (the consultant, 2026-11-19: Imaginarium's R-1).
#[tokio::test]
async fn a_tag_pushed_by_hand_marks_its_release_tagged() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let call = |tool: &'static str, args: Value| {
        let (s, token, sess) = (&s, &token, &sess);
        async move { mcp_tool(s, token, sess, tool, args).await }
    };
    assert!(call("update_project", json!({"repo": "giosuel/vinculum"})).await.0);
    assert!(call("add_milestone", json!({"name": "M4", "title": "Keybinds"})).await.0);
    let (ok, r) = call("add_release", json!({"tag": "v0.0.10-m4", "title": "M4", "milestone": "M4"})).await;
    assert!(ok, "{r}");
    let (id, button) = (r["id"].as_str().unwrap().to_owned(), r["tag_button"]["id"].as_i64().unwrap());

    // Not on GitHub yet: it waits.
    s.github.tags.lock().unwrap().insert(("giosuel/vinculum".into(), "v0.0.9-m3".into()));
    crate::poller::check_pushed_tags(&s.state).await.unwrap();
    assert_eq!(call("get_record", json!({"id": id})).await.1["state"], "waiting");

    s.github.tags.lock().unwrap().insert(("giosuel/vinculum".into(), "v0.0.10-m4".into()));
    crate::poller::check_pushed_tags(&s.state).await.unwrap();
    let (_, got) = call("get_record", json!({"id": id})).await;
    assert_eq!(got["state"], "tagged", "{got}");
    let (_, m) = call("get_milestone", json!({"milestone": "M4"})).await;
    assert_eq!(m["state"], "closed", "{m}");
    let row = s.state.db.call(move |c| crate::actions::get(c, button)).await.unwrap().unwrap();
    assert_eq!(row.state, "withdrawn");
}

async fn owner_page(s: &TestServer, cookie: &str, path: &str) -> String {
    s.http.get(s.url(path)).header(COOKIE, cookie).send().await.unwrap().text().await.unwrap()
}

/// Plan's link to the step the board shows (D-219).
fn step_bar_link(html: &str) -> String {
    let at = html.find(r#"class="btn sm secondary step-open" href=""#).expect("a step line")
        + r#"class="btn sm secondary step-open" href=""#.len();
    html[at..].split('"').next().unwrap().to_owned()
}

/// Waits (up to 10 s) until each PR's record is in `state`, for work that runs after a redirect.
async fn wait_for_records(s: &TestServer, slug: &str, numbers: &[i64], state: &str) {
    for _ in 0..200 {
        let (slug, numbers, want) = (slug.to_owned(), numbers.to_vec(), state.to_owned());
        let all = s
            .state
            .db
            .call(move |c| {
                let p = crate::projects::require(c, &slug)?;
                let mut all = true;
                for n in numbers {
                    let st: String = c.query_row(
                        "SELECT state FROM records WHERE project_id = ?1 AND kind = 'pr' AND number = ?2",
                        rusqlite::params![p.id, n],
                        |r| r.get(0),
                    )?;
                    all &= st == want;
                }
                anyhow::Ok(all)
            })
            .await
            // The tests' in-memory database shares its cache: a read that meets the merge's write
            // fails at once ("table is locked") instead of waiting. Not yet, then.
            .unwrap_or(false);
        if all {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("PRs {numbers:?} of {slug} never became {state}");
}

/// D-263: a button's confirmation says a command runs. AgentFlow's own Merge, "Merge #a–#b" and
/// Push tag say what they do (the same command every time), with the agent's message.
#[tokio::test]
async fn confirmations_say_what_runs() {
    use crate::github::fake::pr;
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let call = |name: &'static str, args: Value| {
        let (s, token, sess) = (&s, token.clone(), sess.clone());
        async move { mcp_tool(s, &token, &sess, name, args).await }
    };
    call("update_project", json!({"repo": "giosuel/vinculum"})).await;
    s.github.set("giosuel/vinculum", pr(48, "main", "b48"));
    s.github.set("giosuel/vinculum", pr(49, "b48", "b49"));
    crate::poller::poll_once(&s.state).await.unwrap();
    let (ok, r) = call(
        "add_release",
        json!({"tag": "v0.1.0", "title": "Vinculum 0.1.0", "body": "Keybinds.\n\n- Saved per app"}),
    )
    .await;
    assert!(ok, "{r}");
    let cookie = s.owner_cookie().await;
    let html =
        s.http.get(s.url("/p/vinculum?tab=prs")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(
        html.contains(r#"data-confirm="Merge #48?" data-confirm-body>Merge</button><template class="confirm-body">"#),
        "{html}"
    );
    assert!(html.contains("merge command for each PR in giosuel/vinculum, one after the other"), "the stack in words");
    assert!(html.contains(r#"data-confirm="Push tag v0.1.0?" data-confirm-body>"#));
    assert!(html.contains("Message, written by vinculum"));
    assert!(
        html.contains("<pre class=\"cmd wrap msg\">Vinculum 0.1.0\n\nKeybinds.\n\n- Saved per app</pre>"),
        "{html}"
    );
    assert!(html.contains("<summary class=\"sub\">The exact command</summary>"));
}

/// The owner, 2026-11-18 (D-214, D-215): a stacked PR merges only with the PRs below it, and red
/// CI in the middle doesn't hold the stack when the PR it ends at is green.
#[tokio::test]
async fn a_stack_merges_past_red_ci_in_the_middle_only_up_to_a_green_pr() {
    use crate::github::fake::pr;
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let call = |name: &'static str, args: Value| {
        let (s, token, sess) = (&s, token.clone(), sess.clone());
        async move { mcp_tool(s, &token, &sess, name, args).await }
    };
    call("update_project", json!({"repo": "giosuel/vinculum"})).await;
    // #61 on main, #62 (red) on #61, #63 (green) on #62, #64 (red) on #63.
    let mut red62 = pr(62, "b61", "b62");
    red62.ci = "failing".into();
    let mut red64 = pr(64, "b63", "b64");
    red64.ci = "failing".into();
    for p in [pr(61, "main", "b61"), red62, pr(63, "b62", "b63"), red64] {
        s.github.set("giosuel/vinculum", p);
    }
    crate::poller::poll_once(&s.state).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let wd = dir.path().display().to_string();
    for n in [61, 62, 63, 64] {
        let (ok, r) = call("add_button", json!({"record": format!("PR-{n}"), "label": "Merge", "kind": "merge", "command": format!("echo merged {n} > merged-{n}"), "workdir": wd})).await;
        assert!(ok, "{r}");
    }

    let cookie = s.owner_cookie().await;
    let html =
        s.http.get(s.url("/p/vinculum?tab=prs")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(html.contains("Merge #61–#63"), "past #62's red CI to the green #63");
    assert!(!html.contains("Merge #61–#62") && !html.contains("Merge #61–#64"), "never ending at a red PR");
    assert!(html.contains("<span class=\"sub\">CI failing</span>"), "#64 says why");

    // A page that showed another stack is turned away; so is a chain ending at a red PR.
    let start = html.find(r#"name="actions" value=""#).unwrap() + r#"name="actions" value=""#.len();
    let ids = html[start..].split('"').next().unwrap().to_owned();
    let post = |to: i64, ids: String| {
        let (s, cookie) = (&s, cookie.clone());
        async move {
            s.http
                .post(s.url(&format!("/p/vinculum/merge-up-to/{to}")))
                .header(COOKIE, &cookie)
                .header(ORIGIN, &s.base)
                .form(&[("actions", ids.as_str())])
                .send()
                .await
                .unwrap()
        }
    };
    let stale = ids.rsplit_once(',').unwrap().0.to_owned();
    assert_eq!(post(63, stale).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(post(64, ids.clone()).await.status(), StatusCode::BAD_REQUEST);
    assert!(!dir.path().join("merged-61").exists(), "nothing ran");

    s.github.merge_on_fetch.lock().unwrap().extend([61, 62, 63]);
    let res = post(63, ids).await;
    assert_eq!(res.headers()[LOCATION], "/p/vinculum?tab=prs&merging=61-63");
    wait_for_records(&s, "vinculum", &[61, 62, 63], "merged").await;
    for n in [61, 62, 63] {
        assert!(dir.path().join(format!("merged-{n}")).exists(), "#{n} ran");
    }
    assert!(!dir.path().join("merged-64").exists());
}

/// The owner, 2026-11-19 (D-228–D-230): a subagent's model shows in Conversations, a note it
/// uploaded is tagged as its, and record ids in its report link to them.
#[tokio::test]
async fn a_subagents_model_note_and_links_show() {
    use crate::sessionlog::{Found, Msg, SubagentInfo, Who};
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let now = crate::util::now_ms();
    // Its transcript: it runs on Fable, uploads handover.md, and reports back naming N-1.
    s.state
        .db
        .call(move |c| {
            let pid: i64 = c.query_row("SELECT id FROM projects WHERE slug = 'vinculum'", [], |r| r.get(0))?;
            let found = Found {
                subagent: Some(SubagentInfo {
                    id: "a1b2c3".into(),
                    parent_session: "s1".into(),
                    description: "Inspector handover".into(),
                    agent_type: "code-review".into(),
                }),
                first_at: Some(now),
                model: Some("claude-fable-5-2".into()),
                note_calls: vec![(now, "handover.md".into())],
                msgs: vec![Msg {
                    at: now,
                    from: Who::Me,
                    to: Who::Parent,
                    body: "Done; the handover is in N-1.".into(),
                }],
                ..Default::default()
            };
            crate::sessionlog::store(c, &found, "a1b2c3", "-home-viber-vinculum", "/home/viber/vinculum", Some(pid))?;
            Ok(())
        })
        .await
        .unwrap();
    // The upload comes after the call was seen; it's tagged when it arrives.
    let res = s
        .http
        .post(s.url("/api/notes?name=Handover&category=handover&filename=handover.md"))
        .bearer_auth(&token)
        .body(&b"# Handover"[..])
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success(), "{}", res.text().await.unwrap());
    let cookie = s.owner_cookie().await;
    let note = owner_page(&s, &cookie, "/p/vinculum/N-1").await;
    assert!(note.contains("its subagent · code-review · Fable 5.2"), "{note}");
    assert!(note.contains("s subagent · "), "its History says who: {note}");
    let notes = owner_page(&s, &cookie, "/p/vinculum?tab=notes").await;
    assert!(notes.contains(r#"<span class="chip info""#), "{notes}");
    let conv = owner_page(&s, &cookie, "/conversations?kind=sub").await;
    assert!(conv.contains(r#"<span class="tag line">Fable 5.2</span>"#), "{conv}");
    assert!(conv.contains(r#"<a href="/p/vinculum/N-1">N-1</a>"#), "{conv}");
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

    // Each item has the owner's verdict right there (Q-8): Works; the agent hears it, with a note.
    assert!(page.contains(r#"action="/p/imaginarium/TT-1/try""#) && page.contains(">Works</button>"), "{page}");
    let mark = |id: &'static str, form: Vec<(&'static str, &'static str)>| {
        let (http, url, cookie, origin) =
            (s.http.clone(), s.url(&format!("/p/imaginarium/{id}/try")), cookie.clone(), s.base.clone());
        async move { http.post(url).header(COOKIE, &cookie).header(ORIGIN, &origin).form(&form).send().await.unwrap() }
    };
    let res = mark("TT-1", vec![("v", "works"), ("note", "crisp"), ("back", "/p/imaginarium/B-1")]).await;
    assert_eq!(res.headers()[LOCATION], "/p/imaginarium/B-1?tried=TT-1.works");
    let page = owner_page(&s, &cookie, "/p/imaginarium/B-1?tried=TT-1.works").await;
    assert!(page.contains("TT-1 works: off the list. The agent hears it."), "{page}");
    let sess = mcp_session(&s, &token).await;
    let (_, news) = mcp_tool(&s, &token, &sess, "since_last_check", json!({})).await;
    assert!(news.to_string().contains("Owner tested, works: TT-1 Tables. Note: \\\"crisp\\\""), "{news}");
    // Done is done; a problem needs the owner's words, and the item stays open with them.
    assert_eq!(mark("TT-1", vec![("v", "skip")]).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(mark("TT-2", vec![("v", "problem"), ("note", " ")]).await.status(), StatusCode::BAD_REQUEST);
    let res = mark("TT-2", vec![("v", "problem"), ("note", "blurry at 200 %")]).await;
    assert_eq!(res.headers()[LOCATION], "/p/imaginarium?tab=builds&tried=TT-2.problem");
    let (_, news) = mcp_tool(&s, &token, &sess, "since_last_check", json!({})).await;
    assert!(news.to_string().contains("Owner found a problem in TT-2 Pictures"), "{news}");

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
    let (ok, r) =
        mcp_tool(&s, &token, &sess, "add_to_test", json!({"title": "Zoom", "category": "Tables", "build": "B-9"}))
            .await;
    assert!(!ok && r.to_string().contains("B-9"), "a build that isn't there: {r}");
    // A new release takes what's open along.
    let (ok, r) = mcp_tool(&s, &token, &sess, "add_release", json!({"tag": "v0.9.0", "title": "M9"})).await;
    assert!(ok, "{r}");
    assert_eq!(r["to_test"], 2);
    // The Builds tab: the newest build with what came with it, the rest still to try. To test is
    // part of it now (Q-6): its old links open Builds.
    let page = owner_page(&s, &cookie, "/p/imaginarium?tab=builds").await;
    assert!(page.contains("New in B-1") && page.contains("2 of 2 tried"), "{page}");
    assert!(page.contains("blurry at 200 % (giosuel, ") && page.contains("the border is cut off"), "{page}");
    assert!(page.contains("Still to try") && page.contains("Undo a table edit"), "{page}");
    assert!(!page.contains(r#"class="tt-t">Tables</span>"#), "tested, off the list: {page}");
    assert!(page.contains("<pre class=\"cmd\">flow run imaginarium 1</pre>"), "{page}");
    let found = owner_page(&s, &cookie, "/p/imaginarium?tab=builds&tstate=tested").await;
    assert!(found.contains(r#"class="tt-t">Tables</span>"#) && found.contains("works · "), "{found}");
    let old = s.http.get(s.url("/p/imaginarium?tab=to-test")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(old.headers()[LOCATION], "/p/imaginarium?tab=builds");

    // A Windows build: not for flow run, a download on its page, with what it needs.
    let exe = tempfile::tempdir().unwrap();
    std::fs::write(exe.path().join("lumen.exe"), b"MZ fake exe").unwrap();
    let mut wtar = Vec::new();
    crate::builds::pack(&exe.path().join("lumen.exe"), &mut wtar).unwrap();
    let wurl =
        |q: &str| s.url(&format!("/api/builds?product=imaginarium&summary=Windows&commit=b2c3d4e&branch=m8&{q}"));
    let res = s
        .http
        .post(wurl("entry=lumen.exe&platform=windows%20x86"))
        .bearer_auth(&token)
        .body(wtar.clone())
        .send()
        .await
        .unwrap();
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
    let list: Value =
        s.http.get(s.url("/api/builds/imaginarium")).bearer_auth(&device).send().await.unwrap().json().await.unwrap();
    let w = list["builds"].as_array().unwrap().iter().find(|b| b["number"] == n).cloned().unwrap();
    assert_eq!(w["platform"], "windows-x86_64");
    let cookie = s.owner_cookie().await;
    let page = s
        .http
        .get(s.url(&format!("/p/imaginarium/B-{n}")))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("Windows · x86_64")
            && page.contains("/download")
            && page.contains("Vulkan")
            && !page.contains(&format!("flow run imaginarium {n}")),
        "{page}"
    );
    let file =
        s.http.get(s.url(&format!("/p/imaginarium/B-{n}/download"))).header(COOKIE, &cookie).send().await.unwrap();
    assert!(file.headers()["content-disposition"].to_str().unwrap().contains("lumen.exe"));
    assert_eq!(file.bytes().await.unwrap().as_ref(), b"MZ fake exe");
    // On the Builds tab it's the newest build, a download only, and what was added without a
    // build since the one before goes with it (Q-8: matched by publish time). B-1 is earlier now,
    // with your verdicts, its run command and a download (D-243); a Linux build downloads too.
    let page = owner_page(&s, &cookie, "/p/imaginarium?tab=builds").await;
    let new = &page[page.find(r#"class="bn-new""#).unwrap()..page.find(r#"id="to-try""#).unwrap()];
    assert!(new.contains(&format!("New in B-{n}")) && new.contains("Undo a table edit"), "{new}");
    assert!(
        page.contains(&format!(r#"<a class="btn secondary" href="/p/imaginarium/B-{n}/download" download>"#))
            && page.contains("needs Vulkan")
            && !page.contains(&format!("flow run imaginarium {n}")),
        "{page}"
    );
    let earlier = &page[page.find(r#"id="earlier""#).unwrap()..];
    assert!(
        earlier.contains("flow run imaginarium 1") && earlier.contains("1 works") && earlier.contains("1 problem<"),
        "{earlier}"
    );
    assert!(earlier.contains(r#"href="/p/imaginarium/B-1/download""#), "{earlier}");
    // Every earlier build is in the page, ten at a time with the pager component (no "Show all").
    assert!(earlier.contains(r#"data-pager="10""#) && earlier.contains(r#"<div class="pager" hidden>"#), "{earlier}");
    assert!(!earlier.contains("Show all") && !earlier.contains("ball=1"), "{earlier}");
    let linux = s.http.get(s.url("/p/imaginarium/B-1/download")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(linux.status(), StatusCode::OK);
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
    let post = |q: String| {
        s.http
            .post(s.url(&format!("/api/builds?product=imaginarium&summary=Old%20one&{q}")))
            .bearer_auth(&token)
            .body(tar.clone())
            .send()
    };

    // viber-run's #63, then its #5 from October: each keeps its number, date and name.
    let res: Value =
        post("commit=79916898&branch=feat%2Fstroke-dash-gap&built_at=2026-11-11%2021%3A37&number=63".into())
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(res["number"], 63, "{res}");
    assert_eq!(res["name"], "2026-11-11-2137-stroke-dash-gap-79916898");
    let res: Value =
        post("commit=2925797&branch=feat%2Fm6.12-text-style&built_at=2026-10-26%2010%3A28&number=5".into())
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(res["number"], 5, "{res}");
    assert_eq!(res["name"], "2026-10-26-1028-m6.12-text-style-2925797");

    // A taken number, a number without a date, a date ahead, and something to try are refused.
    let res = post("commit=2925797&built_at=2026-10-26%2010%3A28&number=5".into()).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    for q in [
        "commit=2925797&number=6",
        "commit=2925797&built_at=2099-01-01%2000%3A00&number=6",
        "built_at=2026-10-26%2010%3A28&number=6&features=%5B%22A%3A%20b%22%5D",
        "built_at=yesterday&number=6",
    ] {
        assert_eq!(post(q.into()).await.unwrap().status(), StatusCode::BAD_REQUEST, "{q}");
    }

    // The newest is still the newest, the October one is dated October, and the next new build
    // continues after the highest number.
    let list: Value =
        s.http.get(s.url("/api/builds/imaginarium")).bearer_auth(&device).send().await.unwrap().json().await.unwrap();
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
        s.http
            .post(s.url(&format!(
                "/api/builds?product=imaginarium&summary={summary}&built_at=2026-10-27%2010%3A00&number=7&check=true"
            )))
            .bearer_auth(&token)
            .send()
    };
    let ok: Value = ask(long.clone()).await.unwrap().json().await.unwrap();
    assert_eq!(ok["ok"], true);
    assert_eq!(ask("y".repeat(301)).await.unwrap().status(), StatusCode::BAD_REQUEST);
    let res = s
        .http
        .post(s.url("/api/builds?product=imaginarium&summary=x&built_at=2026-10-27%2010%3A00&number=5&check=true"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT, "a taken number is known before the upload");
    let res: Value = s
        .http
        .post(s.url(&format!(
            "/api/builds?product=imaginarium&summary={long}&commit=abcdef1&built_at=2026-10-27%2010%3A00&number=7"
        )))
        .bearer_auth(&token)
        .body(tar.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["number"], 7, "{res}");

    // Nothing was put on the To test list for the old ones.
    let tests: i64 = s
        .state
        .db
        .call(|c| Ok(c.query_row("SELECT count(*) FROM records WHERE kind = 'test'", [], |r| r.get(0))?))
        .await
        .unwrap();
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
    assert!(
        page.contains("Between sessions") && page.contains("Subagents") && page.contains("Scopes are live."),
        "{page}"
    );
    // Subagents, by their session (2026-11-18): one run at a time.
    let subs = s
        .http
        .get(s.url("/conversations?kind=sub"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        subs.contains("Subagents, by session") && subs.contains("Review 1: security") && subs.contains("Run 1 of 1"),
        "{subs}"
    );
    assert!(subs.contains(">told<") && subs.contains(">reported<") && subs.contains("Worked 20 min"), "{subs}");
    // Participant and search narrow the list.
    let get = |path: &'static str| {
        let (s, cookie) = (&s, cookie.clone());
        async move { s.http.get(s.url(path)).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap() }
    };
    let p = get("/conversations?who=agent-flow").await;
    assert!(p.contains("With agent-flow") && p.contains("1 pair · 2 messages"), "{p}");
    assert!(get("/conversations?who=nobody").await.contains("Nothing matches."));
    assert!(get("/conversations?q=thanks").await.contains("Conversation 1 of 1"));
    assert!(get("/conversations?q=from:nobody").await.contains("Nothing matches."));
    // Three panes (2026-11-18): the pair's conversations by day in the middle; on the phone, the
    // pair's list is the second level and a conversation the third.
    let mid = get("/conversations?t=pair%3Aagent-flow%7Cimaginarium").await;
    assert!(
        mid.contains("cv-mid")
            && mid.contains("class=\"cv-dh\"")
            && mid.contains("12 Nov")
            && mid.contains("1 conversation<"),
        "{mid}"
    );
    assert!(mid.contains("cv3 picked\"") && !mid.contains("opened"), "the middle level on the phone");
    assert!(mid.contains("data-find=\"scopes are live."), "{mid}");
    let one = get("/conversations?t=pair%3Aagent-flow%7Cimaginarium&c=1").await;
    assert!(one.contains("cv3 picked opened") && one.contains("Conversation 1 of 1"), "{one}");
    assert!(get("/conversations?q=nothing-like-this").await.contains("Nothing matches."));
    let pair = s
        .http
        .get(s.url("/conversations?t=pair%3Aagent-flow%7Cimaginarium"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(pair.matches("<p>Scopes are live.</p>").count(), 1, "kept once: {pair}");
    assert!(pair.contains("<strong>thanks</strong>") && pair.contains("› agent-flow"), "{pair}");
    assert!(pair.contains("Conversation 1 of 1") && pair.contains("This is the newest conversation"), "{pair}");
    let sub = s
        .http
        .get(s.url("/conversations?t=sub%3Aa0ca2a7e879b8b08a"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(sub.contains("Review the diff for security.") && sub.contains("No findings worth fixing."), "{sub}");
    assert!(!sub.contains("secret file contents"), "tool results stay out");
    assert!(!page.contains("<textarea"), "no reply box");
    // Reload wakes the reader; tokens can't use the page or the button.
    let res = s
        .http
        .post(s.url("/transcripts/read"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("next", "/conversations")])
        .send()
        .await
        .unwrap();
    assert!(res.status().is_redirection(), "{}", res.status());
    let v = s.token(TokenKind::Project, Some("vinculum")).await;
    assert_eq!(
        s.http.get(s.url("/conversations")).bearer_auth(&v).send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert!(!s.http.post(s.url("/transcripts/read")).bearer_auth(&v).send().await.unwrap().status().is_success());
}

#[tokio::test]
async fn the_owner_and_the_consultant_edit_the_handbook_and_agents_read_it() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let (_, list) = mcp_tool(&s, &token, &sess, "handbook", json!({})).await;
    assert_eq!(list["pages"][0]["sections"], json!(["Introduction", "Ports"]), "{list}");
    let (ok, err) =
        mcp_tool(&s, &token, &sess, "handbook_read", json!({"name": "MACHINE.md", "section": "Nope"})).await;
    assert!(!ok && err.as_str().unwrap().contains("it has: Introduction, Ports"));
    let (ok, _) = mcp_tool(&s, &token, &sess, "handbook_read", json!({"name": "../secret.md"})).await;
    assert!(!ok);
    // Only the consultant edits through MCP.
    let (ok, err) =
        mcp_tool(&s, &token, &sess, "handbook_edit", json!({"name": "MACHINE.md", "text": "x", "what": "x"})).await;
    assert!(!ok && err.as_str().unwrap().contains("only the consultant"), "{err}");

    let cookie = s.owner_cookie().await;
    let get = |path: &'static str| {
        let (s, cookie) = (&s, cookie.clone());
        async move { s.http.get(s.url(path)).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap() }
    };
    let page = get("/handbook").await;
    assert!(
        page.contains("<h1>Handbook</h1>") && page.contains("MACHINE.md") && !page.contains("pull request"),
        "{page}"
    );
    // The owner edits the whole file in the Markdown editor, like the project document (D-211).
    let page = get("/handbook?f=MACHINE.md&edit=1").await;
    assert!(page.contains("data-md-editor") && page.contains(r#"name="base_rev" value="1""#), "{page}");
    let save = |base: i64, text: &'static str| {
        let (s, cookie) = (&s, cookie.clone());
        async move {
            s.http
                .post(s.url("/handbook"))
                .header(COOKIE, &cookie)
                .header(ORIGIN, &s.base)
                .form(&[("f", "MACHINE.md"), ("base_rev", &base.to_string()), ("text", text)])
                .send()
                .await
                .unwrap()
        }
    };
    let res = save(1, "# MACHINE\n\nThe server.\n\n## Ports\n\nUse 10.0.8.1 or 127.0.0.1.\n").await;
    assert_eq!(res.headers()[LOCATION], "/handbook?f=MACHINE.md&saved=2");
    let (_, ports) =
        mcp_tool(&s, &token, &sess, "handbook_read", json!({"name": "MACHINE.md", "section": "ports"})).await;
    assert!(ports["text"].as_str().unwrap().contains("127.0.0.1"));
    // Written out as a file too.
    let file = std::fs::read_to_string(s.state.config.data_dir().join("handbook/MACHINE.md")).unwrap();
    assert!(file.contains("127.0.0.1"));
    assert!(get("/handbook?f=MACHINE.md&history=1").await.contains("owner: Ports"));
    let preview = s
        .http
        .post(s.url("/handbook/preview"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("text", "See [OWNER.md](OWNER.md).")])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(preview.contains(r#"href="/handbook?f=OWNER.md""#), "{preview}");

    // The consultant edits a section with the version it read; a stale one is refused.
    let admin = s.token(TokenKind::Admin, None).await;
    let asess = mcp_session(&s, &admin).await;
    let (_, read) = mcp_tool(&s, &admin, &asess, "handbook_read", json!({"name": "MACHINE.md"})).await;
    let v = read["version"].as_str().unwrap().to_owned();
    let (ok, r) = mcp_tool(&s, &admin, &asess, "handbook_edit", json!({"name": "MACHINE.md", "version": v, "section": "Ports", "text": "## Ports\n\nOnly 10.0.8.1.", "what": "Ports"})).await;
    assert!(ok && r["rev"] == 3, "{r}");
    let (ok, err) = mcp_tool(
        &s,
        &admin,
        &asess,
        "handbook_edit",
        json!({"name": "MACHINE.md", "version": v, "text": "x", "what": "x"}),
    )
    .await;
    assert!(!ok && err.as_str().unwrap().contains("changed since"), "{err}");
    let (ok, _) = mcp_tool(
        &s,
        &admin,
        &asess,
        "handbook_edit",
        json!({"name": "templates/new.md", "text": "# New\n", "what": "a template"}),
    )
    .await;
    assert!(ok);
    // The owner's save from rev 2 after the consultant's rev 3: merged by section, or both versions.
    let res = save(2, "# MACHINE\n\nThe server, viber.\n\n## Ports\n\nUse 10.0.8.1 or 127.0.0.1.\n").await;
    assert_eq!(res.headers()[LOCATION], "/handbook?f=MACHINE.md&saved=4&taken=Ports");
    let (_, now) = mcp_tool(&s, &token, &sess, "handbook_read", json!({"name": "MACHINE.md"})).await;
    let text = now["text"].as_str().unwrap();
    assert!(text.contains("viber") && text.contains("Only 10.0.8.1."), "{text}");
    let page = save(3, "# MACHINE\n\nThe server, mine.\n\n## Ports\n\nOnly 10.0.8.1.\n").await.text().await.unwrap();
    assert!(page.contains("Use its version") && page.contains(r#"name="base_rev" value="4""#), "{page}");

    // Machines fetch it whole, or hear it's unchanged.
    let b: Value = s.http.get(s.url("/api/handbook")).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    assert_eq!(b["files"].as_array().unwrap().len(), 2, "{b}");
    let v = b["version"].as_str().unwrap();
    let again: Value = s
        .http
        .get(s.url(&format!("/api/handbook?have={v}")))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again["unchanged"], true);
    assert_eq!(s.http.get(s.url("/api/handbook")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_dashboard_has_an_overview_of_the_plan_today_the_machine_and_builds() {
    let s = TestServer::start().await;
    let cookie = s.owner_cookie().await;
    let html = s.http.get(s.url("/")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    for part in [
        "ov-plan",
        "No status line has sent the plan",
        "ov-today",
        "$0.00",
        "ov-machine",
        "Memory",
        "/tmp",
        "agents online",
    ] {
        assert!(html.contains(part), "{part}: {html}");
    }
    // With a plan sample, the windows show with the mark of their time.
    let now = crate::util::now_ms();
    s.db.call(move |c| {
        let l = crate::plan::Limits {
            five_hour: Some(crate::plan::Window { used_percentage: 42.0, resets_at: now / 1000 + 3_600 }),
            seven_day: Some(crate::plan::Window { used_percentage: 71.0, resets_at: now / 1000 + 2 * 86_400 }),
        };
        crate::plan::store(c, &l, now, None, "m")?;
        Ok(())
    })
    .await
    .unwrap();
    let html = s.http.get(s.url("/")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(
        html.contains("42% · ")
            && html.contains("71% · ")
            && html.contains("<span class=\"mark\" style=\"left: 80%\">"),
        "{html}"
    );
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

/// Writes a Log entry by the owner, and returns its seq.
async fn log_entry(s: &TestServer, action: crate::activity::Action, summary: &str) -> i64 {
    let summary = summary.to_owned();
    s.db.call(move |c| {
        let owner = crate::auth::Actor::Owner {
            session_id: 1,
            method: "passkey".into(),
            person: 1,
            admin: true,
            ready: true,
            name: "giosuel".into(),
        };
        let e = crate::activity::Entry::new(&owner, crate::activity::Via::Page, action, summary);
        crate::activity::write_conn(c, &e)?;
        crate::logview::latest_seq(c)
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn archived_reads_take_their_place_among_older_entries() {
    use crate::activity::Action;
    let s = TestServer::start().await;
    let cookie = s.owner_cookie().await;
    // A change, thirty reads, a change: the reads go to the archive, the changes stay.
    log_entry(&s, Action::Write, "probe first").await;
    for i in 0..30 {
        log_entry(&s, Action::Read, &format!("probe read {i}")).await;
    }
    log_entry(&s, Action::Write, "probe last").await;
    let data = s.state.config.data_dir();
    s.db.call(move |c| {
        c.execute("UPDATE log SET at = at - 100 * 86400000 WHERE summary LIKE 'probe %'", [])?;
        crate::logview::archive_old_reads(c, &data, crate::util::now_ms())
    })
    .await
    .unwrap();
    let html = s
        .http
        .get(s.url("/log/rows?q=probe&range=all"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(html.matches("data-seq=").count(), 32, "{html}");
    assert_eq!(html.matches("<span class=\"tag\">archived</span>").count(), 30);
    // Newest first, the archived reads between the two changes.
    let first = html.find("probe first").unwrap();
    assert!(html.find("probe last").unwrap() < html.find("probe read 29").unwrap());
    assert!(html.find("probe read 0").unwrap() < first);
}

#[tokio::test]
async fn the_live_log_picks_up_where_it_left_off() {
    use crate::activity::Action;
    let s = TestServer::start().await;
    let cookie = s.owner_cookie().await;
    let seen = log_entry(&s, Action::Write, "probe shown before").await;
    // EventSource reconnects to the URL it opened, and says which event it got last.
    let mut res = s
        .http
        .get(s.url("/log/stream?after=0"))
        .header(COOKIE, &cookie)
        .header("last-event-id", seen.to_string())
        .send()
        .await
        .unwrap();
    log_entry(&s, Action::Write, "probe new").await;
    let mut got = String::new();
    while !got.contains("probe new") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(10), res.chunk()).await.expect("an event");
        got.push_str(&String::from_utf8_lossy(&chunk.unwrap().expect("still open")));
    }
    assert!(!got.contains("probe shown before"), "{got}");
    assert!(got.contains("\nid: ") || got.starts_with("id: "), "events carry their seq: {got}");
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
    // One page (D-238): no Overview | Tasks tabs; the tasks are a card, all of them a dialog.
    assert!(!html.contains(r#"<nav class="tabs" aria-label="Usage">"#), "{html}");
    assert!(html.contains(r#"<h2 id="tasks-h">Tasks by cost</h2>"#), "{html}");
    assert!(html.contains("No tokens are counted against a task yet"), "{html}");
    assert!(html.contains(r#""Not in a task" $"#), "{html}");
    assert!(html.contains(r#"<a class="tk-all" href="/usage/tasks" data-dlg>"#), "{html}");
    assert!(html.contains(r#"href="/usage?days=14#tasks-h""#), "{html}");
    // Its tables sort by their headers (the table component): Per project, by cost too.
    assert!(html.contains(r#"<table class="table nums" data-sortable>"#), "{html}");
    assert!(html.contains(r#"<button type="button" class="th-sort" data-sort="num">Cost<span class="sort-i" aria-hidden="true"></span></button>"#), "{html}");
    let all = get("/usage/tasks").await.text().await.unwrap();
    assert!(
        all.contains("<div data-dlg-content data-dlg-size=\"lg\" class=\"tasks-all\">")
            && all.contains("Not in a task"),
        "{all}"
    );
    assert!(all.contains("in the last 7 days, last worked on first"), "{all}");
    assert!(all.contains(r#"<th scope="col" class="hide-phone tk-l" aria-sort="descending" data-default="descending"><button type="button" class="th-sort" data-sort="num">Worked"#), "{all}");
    assert!(all.contains("<tfoot><tr class=\"tk-out\">"), "Not in a task stays under the tasks: {all}");
    assert!(get("/usage/tasks?days=14").await.text().await.unwrap().contains("in the last 14 days"));
    // The old Tasks tab's address leads there.
    let old = get("/usage?tab=tasks&days=14").await;
    assert!(old.status().is_redirection());
    assert_eq!(old.headers()[reqwest::header::LOCATION], "/usage/tasks?days=14");
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
    let res = get("/machines").await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = res.text().await.unwrap();
    // AgentFlow's figures, live (machine.js swaps the parts marked data-live in).
    assert!(html.contains("GB free of") && html.matches(r#"<div class="card pad mtile"#).count() == 5, "{html}");
    assert!(html.contains(r#"<div class="mtiles" id="tiles" data-live"#), "{html}");
    let res = s.http.get(s.url("/machines")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let html = get("/p/imaginarium").await.text().await.unwrap();
    assert!(html.contains("Tokens, 7 days") && html.contains("1.2 M"));
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
    let (ok, err) = call(
        "add_tasks",
        json!({"milestone": "K1", "tasks": [
        {"title": "Parse", "estimate": "2 h"}, {"title": "Store"}, {"title": "Skip me", "state": "skipped"}]}),
    )
    .await;
    assert!(!ok, "{err}");
    let err = err.as_str().unwrap();
    assert!(err.contains("#2: needs an estimate") && err.contains("#3: needs a reason"), "{err}");
    let (ok, r) = call(
        "add_tasks",
        json!({"milestone": "K1", "tasks": [
        {"title": "Parse", "estimate": "2 h", "branch": "feat/binds"},
        {"title": "Store", "estimate": "40 min"},
        {"title": "Rename the module", "state": "skipped", "reason": "churn", "importance": "low"}]}),
    )
    .await;
    assert!(ok, "{r}");
    assert_eq!(r["ids"], json!(["T-1", "T-2", "T-3"]));
    // Order, then doing starts the step; one call moves two.
    let (ok, o) = call("order_tasks", json!({"milestone": "K1", "ids": ["T-2"]})).await;
    assert!(ok && o["order"] == json!(["T-2", "T-1", "T-3"]), "{o}");
    let (ok, u) = call("update_task", json!({"ids": ["T-1", "T-2"], "state": "doing"})).await;
    assert!(ok, "{u}");
    assert_eq!(u["started"], json!(["K1", "M4"]));
    // "Working on" comes from the tasks in doing until the agent says more.
    let ov = s
        .http
        .get(s.url("/p/vinculum"))
        .header(COOKIE, s.owner_cookie().await)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
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
    // The overview's board shows the block; the step, opened from the step bar, the rest.
    let cookie = s.owner_cookie().await;
    let overview = owner_page(&s, &cookie, "/p/vinculum/plan/M4?step=K1").await;
    assert!(overview.contains("blocked by Q-1"), "{overview}");
    let step = owner_page(&s, &cookie, &step_bar_link(&overview)).await;
    assert!(step.contains("added · your feedback · from B-3") && step.contains("skipped: churn"), "{step}");
    // Concluding needs the open tasks dealt with.
    let (ok, err) = call("conclude_step", json!({"step": "K1", "summary": "Binds are read."})).await;
    assert!(!ok && err.as_str().unwrap().contains("still has open tasks"), "{err}");
    let (ok, c) =
        call("conclude_step", json!({"step": "K1", "summary": "Binds are read.", "move_open_to": "K2"})).await;
    assert!(ok, "{c}");
    assert_eq!(c["moved"].as_array().unwrap().len(), 3);
    assert!(!call("add_task", json!({"milestone": "K1", "title": "Late", "estimate": "1 h"})).await.0);
    // History brought in: done tasks may go into a concluded step, without estimates; the step
    // and milestone keep their real dates and summary.
    let (ok, r) =
        call("add_tasks", json!({"milestone": "K1", "tasks": [{"title": "Old work", "state": "done"}]})).await;
    assert!(ok, "{r}");
    let (ok, r) = call(
        "update_milestone",
        json!({"milestone": "K1", "started_at": "2026-10-14", "closed_at": "2026-10-20 18:00", "summary": "Imported."}),
    )
    .await;
    assert!(ok, "{r}");
    assert!(
        !call("update_milestone", json!({"milestone": "K2", "closed_at": "2026-10-20"})).await.0,
        "only a closed one"
    );
    assert!(
        !call("update_milestone", json!({"milestone": "K1", "started_at": "2099-01-01"})).await.0,
        "not in the future"
    );
    // A parked task may have no milestone; nothing else may.
    let (ok, r) = call("add_task", json!({"title": "Windows parity", "state": "parked", "estimate": "2 d"})).await;
    assert!(ok, "{r}");
    assert!(!call("add_task", json!({"title": "Loose", "estimate": "1 h"})).await.0);
    let plan = owner_page(&s, &cookie, "/p/vinculum/plan").await;
    assert!(plan.contains("Without a milestone") && plan.contains("/p/vinculum/plan/none"), "a row of its own");
    let loose = owner_page(&s, &cookie, "/p/vinculum/plan/none").await;
    assert!(loose.contains("Without a milestone") && loose.contains("Windows parity"), "{loose}");
    assert!(!loose.contains("data-srail"), "no steps, no rail");
    // A review is a step: reviewers and commit, findings ranked, one skipped.
    assert!(
        !call(
            "add_milestone",
            json!({"name": "R1", "title": "Review", "review": {"reviewers": ["UX"], "commit": "abc1234"}})
        )
        .await
        .0
    );
    let (ok, r) = call("add_milestone", json!({"name": "K9", "title": "End-of-M4 review", "step_of": "M4",
        "review": {"reviewers": ["correctness", "UX"], "commit": "abc1234", "owner_words": "rate the issues by importance"}})).await;
    assert!(ok, "{r}");
    let (ok, r) = call("add_tasks", json!({"milestone": "K9", "tasks": [
        {"title": "Low thing", "estimate": "20 min", "source": "review", "importance": "low"},
        {"title": "Big thing", "estimate": "2 h", "source": "review", "importance": "high"},
        {"title": "Churn", "state": "skipped", "reason": "no behaviour change", "importance": "low", "source": "review"}]})).await;
    assert!(ok, "{r}");
    let overview = owner_page(&s, &cookie, "/p/vinculum/plan/M4?step=K9").await;
    assert!(overview.contains("End-of-M4 review"), "{overview}");
    let page = owner_page(&s, &cookie, &step_bar_link(&overview)).await;
    assert!(page.contains("abc1234"), "{page}");
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
    let (ok, r) = call(
        "edit_project_doc",
        json!({"section": "How to work on it", "text": "- Conventional Commits\n- DCO sign-off"}),
    )
    .await;
    assert!(ok && r["rev"] == 1, "{r}");
    let (ok, r) = call("edit_project_doc", json!({"section": "Roadmap", "text": "- M4 Keybinds (now)"})).await;
    assert!(ok && r["rev"] == 2, "{r}");
    let (ok, err) = call("edit_project_doc", json!({"text": "x".repeat(17 * 1024)})).await;
    assert!(!ok && err.as_str().unwrap().contains("KB"), "{err}");
    let (_, hello) = call("hello", json!({"session": "V", "machine": "m", "model": "o", "effort": "h"})).await;
    let text = hello["doc"]["text"].as_str().unwrap();
    assert!(text.contains("## How to work on it") && text.contains("## Roadmap"), "{text}");
    assert!(hello["project"].get("stack").is_none(), "the stack lives in the document now");

    // The owner reads it in a dialog from the overview (the owner, 2026-11-18) and edits it whole.
    let cookie = s.owner_cookie().await;
    let get = |path: String| {
        let (s, cookie) = (&s, cookie.clone());
        async move { s.http.get(s.url(&path)).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap() }
    };
    let ov = get("/p/vinculum".into()).await;
    assert!(ov.contains(r#"href="/p/vinculum/doc" data-dlg"#) && ov.contains("rev 2 · "), "{ov}");
    assert!(!get("/p/vinculum?tab=notes".into()).await.contains("Conventional Commits"), "not on the Notes tab");
    let page = get("/p/vinculum/doc".into()).await;
    assert!(
        page.contains("data-dlg-content") && page.contains("Conventional Commits") && page.contains("pd-toc"),
        "{page}"
    );
    assert!(!page.contains("name=\"base_rev\""), "no editor until Edit");
    let page = get("/p/vinculum/doc?edit=1".into()).await;
    assert!(page.contains(r#"name="base_rev" value="2""#) && page.contains("## Roadmap"), "{page}");
    let direct = s.http.get(s.url("/p/vinculum/doc")).header(COOKIE, &cookie).header("sec-fetch-mode", "navigate");
    let direct = direct.send().await.unwrap().text().await.unwrap();
    assert!(direct.contains(r#"data-open-dlg="dlg" data-open-url="/p/vinculum/doc" data-open-from="/p/vinculum""#));
    let post = |base: i64, text: String| {
        let (s, cookie) = (&s, cookie.clone());
        async move {
            s.http
                .post(s.url("/p/vinculum/doc"))
                .header(COOKIE, &cookie)
                .header(ORIGIN, &s.base)
                .form(&[("base_rev", base.to_string()), ("text", text)])
                .send()
                .await
                .unwrap()
        }
    };
    let how = "## How to work on it\n\n- Conventional Commits\n- DCO sign-off\n\n";
    let res = post(2, format!("{how}## Roadmap\n\n- M4 Keybinds\n- M5 Themes\n")).await;
    assert_eq!(res.headers()[LOCATION], "/p/vinculum/doc?done=saved");
    let (_, info) = call("project_info", json!({})).await;
    assert!(info["doc"]["text"].as_str().unwrap().contains("M5 Themes"), "{info}");
    assert_eq!(info["doc"]["rev"], 3);
    // The agent hears which section the owner changed (D-202).
    let (_, news) = call("since_last_check", json!({})).await;
    assert!(news.to_string().contains("Project document: edited Roadmap"), "{news}");
    // The agent saves meanwhile: the owner's save, started from rev 3, takes its section too.
    assert!(call("edit_project_doc", json!({"section": "How to work on it", "text": "- Conventional Commits\n- DCO sign-off\n- Rebase, never merge"})).await.0);
    let res = post(3, format!("{how}## Roadmap\n\n- M4 Keybinds\n- M5 Themes\n- M6 Plugins\n")).await;
    assert_eq!(res.headers()[LOCATION], "/p/vinculum/doc?done=saved&taken=How+to+work+on+it");
    let (_, info) = call("project_info", json!({})).await;
    let text = info["doc"]["text"].as_str().unwrap();
    assert!(text.contains("Rebase, never merge") && text.contains("M6 Plugins"), "{text}");
    // Both changed one section: nothing is saved; the editor shows its version to pick.
    let page = post(3, "## How to work on it\n\n- Squash\n\n## Roadmap\n\n- M4 Keybinds\n".to_owned())
        .await
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("Use its version") && page.contains("- Squash") && page.contains(r#"name="base_rev" value="5""#),
        "{page}"
    );
    assert_eq!(call("project_info", json!({})).await.1["doc"]["rev"], 5);
    let rev = get("/p/vinculum/doc/rev?since=3".into()).await;
    assert!(rev.contains(r#""rev":5"#) && rev.contains("How to work on it"), "{rev}");
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
    assert!(
        up("replaces=N-1&filename=report.md&name=&category=", b"# Done\n\n- T-4\n- T-5").await.status().is_success()
    );
    // A text note that comes back until resolved, one on a past date, one later.
    let (ok, n) = call("add_note", json!({"name": "Loro needs im", "category": "known issue", "text": "Check on every Loro update.", "resurface": "when Loro updates"})).await;
    assert!(ok, "{n}");
    assert!(
        call(
            "add_note",
            json!({"name": "x-wing pin", "category": "known issue", "text": "pinned", "resurface": "2020-01-01"})
        )
        .await
        .0
    );
    assert!(
        call(
            "add_note",
            json!({"name": "Later", "category": "known issue", "text": "later", "resurface": "2099-01-01"})
        )
        .await
        .0
    );
    let (_, hello) = call("hello", json!({"session": "V", "machine": "m", "model": "o", "effort": "h"})).await;
    let mut back: Vec<&str> =
        hello["notes_back"].as_array().unwrap().iter().map(|n| n["name"].as_str().unwrap()).collect();
    back.sort_unstable();
    assert_eq!(back, ["Loro needs im", "x-wing pin"], "{hello}");
    assert!(!call("update_note", json!({"id": "N-3", "state": "resolved"})).await.0, "a reason");
    assert!(call("update_note", json!({"id": "N-3", "state": "resolved", "note": "Loro 1.3 dropped im"})).await.0);
    assert!(!call("update_note", json!({"id": "N-1", "text": "x"})).await.0, "files get a new version instead");

    let cookie = s.owner_cookie().await;
    // Its History (the owner, 2026-11-18): one short line per change, the file it replaced linked,
    // no "changed the size" noise; on the note's page it stands open, without a box (D-268).
    let n1 = owner_page(&s, &cookie, "/p/vinculum/N-1").await;
    assert!(
        n1.contains(r#"<section class="changes" aria-labelledby="changes-h-N-1">"#) && n1.contains("2 changes"),
        "{n1}"
    );
    assert!(n1.contains(r#"href="/p/vinculum/N-1/file?v=1""#) && n1.contains("Created the note"), "{n1}");
    assert!(!n1.contains("changed the size") && !n1.contains(">size<") && !n1.contains("report.md)"), "{n1}");
    let tab =
        s.http.get(s.url("/p/vinculum?tab=notes")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(
        tab.contains("Morning report") && tab.contains("morning report") && tab.contains("1 resolved hidden"),
        "{tab}"
    );
    assert!(
        !tab.contains("Loro needs im") && tab.contains("Comes back 2099-01-01"),
        "resolved notes are hidden: {tab}"
    );
    assert!(tab.contains(r#"nic t-violet"#) && tab.contains(r#"nic t-teal"#), "icons by file type: {tab}");
    // Search and filters like the Log (the owner, 2026-11-18).
    let notes = |q: &'static str| {
        let (s, cookie) = (&s, cookie.clone());
        async move {
            s.http
                .get(s.url(&format!("/p/vinculum?tab=notes&{q}")))
                .header(COOKIE, &cookie)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    };
    let t = notes("nq=loro&nstate=all").await;
    assert!(t.contains("<mark>Loro</mark> needs im") && !t.contains("x-wing pin"), "{t}");
    let t = notes("nq=is:resolved").await;
    assert!(t.contains("Loro needs im") && !t.contains("Later"), "{t}");
    let t = notes("nkind=file").await;
    assert!(t.contains("Morning report") && !t.contains("x-wing pin") && t.contains("md · "), "{t}");
    let t = notes("ncat=known+issue").await;
    assert!(t.contains("x-wing pin") && !t.contains(">Morning report<"), "{t}");
    assert!(t.contains(r#"href="/p/vinculum?tab=notes""#), "Clear: {t}");
    // Sorted by a click on a column's header, in the page: the table component (2026-11-23);
    // newest first by default, and every row carries what its columns sort by.
    let t = notes("nstate=all").await;
    assert!(t.contains(r#"<div class="ntable" role="table" aria-label="Notes" data-sortable>"#), "{t}");
    assert!(
        t.contains(r#"<button type="button" class="th-sort nh" data-sort="text">Title<span class="sort-i""#),
        "{t}"
    );
    assert!(
        t.contains(r#"<span role="columnheader" aria-sort="descending" data-default="descending"><button type="button" class="th-sort nh" data-sort="num">Added"#),
        "{t}"
    );
    assert!(t.contains(r#"<div class="nt" role="cell" data-v="Later">"#), "{t}");
    assert!(t.contains(r#"class="when hide-phone" role="cell" data-v=""#), "{t}");
    assert!(!t.contains("nsort"), "{t}");
    let t = notes("nq=%3Cscript").await;
    assert!(!t.contains("<script>alert") && t.contains("Nothing matches"), "{t}");
    let page = s.http.get(s.url("/p/vinculum/N-1")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("<li>T-5</li>") && page.contains("v1"), "the newest version rendered: {page}");
    // A note is a page of its own (the owner, 2026-11-25, D-264): its file's head with its name,
    // size and version (no version menu, D-265), who wrote it said plainly (D-269).
    assert!(page.contains(r#"<span class="mono ndoc-name">report.md</span>"#), "{page}");
    assert!(page.contains("<span class=\"sub\">version 2</span>") && !page.contains("of 2<"), "{page}");
    assert!(page.contains("vinculum&#39;s project agent"), "{page}");
    // Opened directly (a link in a notification, a reload), it's the note's own page, not a
    // dialog over its project's.
    let direct = |path: &'static str| {
        s.http.get(s.url(path)).header(COOKIE, &cookie).header("sec-fetch-mode", "navigate").send()
    };
    let note = direct("/p/vinculum/N-1").await.unwrap().text().await.unwrap();
    assert!(!note.contains("data-open-dlg") && note.contains("<title>N-1") && note.contains("<li>T-5</li>"), "{note}");
    // A text note is shown like a Markdown file (D-267): N-3.md, its text rendered, Download gives
    // that file; resolved, it says so in About and doesn't come back.
    let n3 = owner_page(&s, &cookie, "/p/vinculum/N-3").await;
    assert!(n3.contains(r#"<span class="mono ndoc-name">N-3.md</span>"#), "{n3}");
    assert!(n3.contains(r#"<div class="ndoc-body prose"><p>Check on every Loro update.</p>"#), "{n3}");
    assert!(n3.contains(r#"<span class="chip ok">resolved</span>"#) && !n3.contains("<dt>Comes back</dt>"), "{n3}");
    let f = s.http.get(s.url("/p/vinculum/N-3/file")).header(COOKIE, &cookie).send().await.unwrap();
    assert!(f.headers()["content-disposition"].to_str().unwrap().contains("N-3.md"));
    assert_eq!(f.text().await.unwrap(), "Check on every Loro update.");
    // A log: its lines numbered, a level in its tone, Wrap and Copy.
    assert!(
        up("name=Dev%20log&category=test%20log&filename=dev.log", b"a INFO x\nb WARN y\n").await.status().is_success()
    );
    let log = owner_page(&s, &cookie, "/p/vinculum/N-6").await;
    assert!(
        log.contains(r#"<span class="cv-n">2</span><span class="cv-t">b <span class="lv-warn">WARN</span> y</span>"#),
        "{log}"
    );
    assert!(log.contains(r#"class="cv-wrap""#) && log.contains(r#"data-copy-from="ndoc-raw""#), "{log}");
    let settings = direct("/settings?s=clients").await.unwrap().text().await.unwrap();
    assert!(settings.contains(r#"data-open-dlg="settings" data-open-url="/settings?s=clients" data-open-from="/""#));
    // The bell's panel and project settings are dialogs only (the owner, 2026-11-18).
    let bell = direct("/notifications").await.unwrap().text().await.unwrap();
    assert!(bell.contains(r#"data-open-dlg="ntf" data-open-url="/notifications" data-open-from="/""#));
    let set = direct("/p/vinculum/settings").await.unwrap().text().await.unwrap();
    assert!(set.contains(r#"data-open-dlg="dlg" data-open-url="/p/vinculum/settings" data-open-from="/p/vinculum""#));
    let old = s.http.get(s.url("/p/vinculum?tab=settings")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(old.headers()[LOCATION], "/p/vinculum/settings");
    let page =
        s.http.get(s.url("/p/vinculum/settings")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("data-dlg-content") && page.contains("Archive the project"), "{page}");
    // Model and Effort are selects from one list (the UI library, M15), with fields of one kind.
    assert!(page.contains(r#"<label class="field">Model<select name="model">"#), "{page}");
    assert!(page.contains(r#"<label class="field">Effort<select name="effort">"#), "{page}");
    assert!(!page.contains("edit.g."), "{page}");
    let f = s.http.get(s.url("/p/vinculum/N-2/file")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(f.headers()[CONTENT_TYPE], "application/octet-stream");
    assert!(f.headers()["content-disposition"].to_str().unwrap().starts_with("attachment"));
    let old = s
        .http
        .get(s.url("/p/vinculum/N-1/file?v=1"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
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
    let (ok, d) = call(
        "record_decision",
        json!({"title": "8990 it is", "owner_words": "yes", "source": "owner", "answers": "Q-1"}),
    )
    .await;
    assert!(ok, "{d}");
    let (_, q1) = call("get_record", json!({"id": "Q-1"})).await;
    assert_eq!(q1["state"], "answered", "{q1}");
    // Answered on the page: the decision is recorded by itself, and the agent hears it with a
    // reminder to check older decisions.
    assert!(
        call("ask_question", json!({"title": "Tabs or a list?", "options": [{"title": "Tabs"}, {"title": "A list"}]}))
            .await
            .0
    );
    let cookie = s.owner_cookie().await;
    let res = s
        .http
        .post(s.url("/p/vinculum/Q-3/answer"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("option", "2")])
        .send()
        .await
        .unwrap();
    assert!(res.status().is_redirection());
    let decisions = s
        .http
        .get(s.url("/p/vinculum?tab=questions"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(decisions.contains("Tabs or a list: A list") && decisions.contains("M4 · "), "{decisions}");
    let (_, news) = call("since_last_check", json!({})).await;
    let news = news.to_string();
    assert!(
        news.contains("Owner answered Q-3: 2: A list → D-")
            && news.contains("check whether it changes an earlier decision"),
        "{news}"
    );
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
    assert!(
        got.to_string().contains("tabs on the left please") && got.to_string().contains("owner asked: recover"),
        "{got}"
    );
    let (ok, r) =
        call("update_decision", json!({"id": id, "state": "archived", "reason": "owner asked: duplicate of D-2"}))
            .await;
    assert!(ok && r["state"] == "archived", "{r}");
    let decisions = s
        .http
        .get(s.url("/p/vinculum?tab=questions"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(decisions.contains("1 archived"), "{decisions}");
    assert!(!call("update_decision", json!({"id": "D-1", "state": "deleted", "reason": "x"})).await.0);
    assert!(
        call("update_decision", json!({"id": id, "state": "current", "reason": "owner: keep it after all"})).await.0
    );

    // A release brings its Push tag button once the project has a repository: GitHub's API makes
    // the tag, so no folder is needed (the project may lie on any machine).
    let (ok, r) = call("add_release", json!({"tag": "v0.0.5-m4", "title": "M4: Keybinds"})).await;
    assert!(ok && r["tag_button"].as_str().unwrap_or("").starts_with("none"), "{r}");
    assert!(call("update_project", json!({"repo": "giosuel/vinculum"})).await.0);
    let (ok, r) = call("add_release", json!({"tag": "v0.0.6-m5", "title": "M5"})).await;
    assert!(ok, "{r}");
    let cmd = r["tag_button"]["command"].as_str().unwrap();
    assert!(
        cmd.contains("repos/giosuel/vinculum/git/ref/heads/main")
            && cmd.contains("-f tag=v0.0.6-m5")
            && cmd.contains("ref=refs/tags/v0.0.6-m5"),
        "{cmd}"
    );
    assert!(!cmd.contains("git push") && !cmd.contains("checkout") && !cmd.contains("cd "), "{cmd}");
    // After a context reset: hello brief leaves out the document and milestones.
    let (_, b) =
        call("hello", json!({"session": "V", "machine": "m", "model": "o", "effort": "h", "brief": true})).await;
    assert!(b.get("doc").is_none() && b.get("milestones").is_none() && b.get("where").is_some(), "{b}");
    assert!(!call("add_release", json!({"tag": "v1; rm -rf ~", "title": "x"})).await.0);
    // Its text can be changed later; its tag can't, and other kinds aren't releases.
    assert!(cmd.contains("msg=$(cat <<'AGENTFLOW_RELEASE_MESSAGE'\nM5\nAGENTFLOW_RELEASE_MESSAGE\n)"), "{cmd}");
    let (ok, u) = call("update_release", json!({"id": r["id"], "body": "Closes M5. Build B-63."})).await;
    assert!(ok, "{u}");
    // The tag carries the title and notes (D-254): new notes, a new Push tag command with them.
    let new_cmd = u["tag_button"]["command"].as_str().unwrap();
    assert!(new_cmd.contains("\nM5\n\nCloses M5. Build B-63.\nAGENTFLOW_RELEASE_MESSAGE\n)"), "{new_cmd}");
    assert!(crate::prs::is_server_command(new_cmd), "the server runs it with its own token");
    let long = "- a line\n".repeat(40);
    assert!(!call("update_release", json!({"id": r["id"], "body": long})).await.0, "the owner reads all of it");
    let (_, got) = call("get_record", json!({"id": r["id"]})).await;
    assert!(got.to_string().contains("Build B-63.") && got.to_string().contains("v0.0.6-m5"), "{got}");
    assert!(!call("update_release", json!({"id": r["id"]})).await.0);
    assert!(!call("update_release", json!({"id": r["id"], "title": " "})).await.0);
    assert!(!call("update_release", json!({"id": "D-1", "body": "x"})).await.0);
    // A question holds a release's tag button until it's answered.
    let rel = r["id"].as_str().unwrap().rsplit('/').next().unwrap().to_owned();
    let (ok, q) =
        call("ask_question", json!({"title": "Tag name: v0.0.6-m5 or v0.1.0?", "blocks": [rel.clone()]})).await;
    assert!(ok, "{q}");
    let page = s
        .http
        .get(s.url(&format!("/p/vinculum/{rel}")))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
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
    let (ok, r) = call(
        "add_to_tests",
        json!({"items": [{"title": "Import", "category": "Import"}, {"title": "Export", "category": "Export"}]}),
    )
    .await;
    assert!(ok && r["ids"].as_array().unwrap().len() == 2, "{r}");

    // A note on a step; a step made a review later; where for a milestone whose steps are done.
    assert!(call("add_milestone", json!({"name": "R9", "title": "Review", "step_of": "M4"})).await.0);
    assert!(
        !call("update_milestone", json!({"milestone": "R9", "review": {"reports": ["N-1"]}})).await.0,
        "not a review yet"
    );
    assert!(
        call("update_milestone", json!({"milestone": "R9", "review": {"reviewers": ["UX"], "commit": "abc1234"}}))
            .await
            .0
    );
    // Only the reports: the reviewers and commit stay.
    assert!(call("update_milestone", json!({"milestone": "R9", "review": {"reports": ["N-26"]}})).await.0);
    let (_, m) = call("get_milestone", json!({"milestone": "R9"})).await;
    assert!(
        m["review"]["reports"] == json!(["N-26"])
            && m["review"]["reviewers"] == json!(["UX"])
            && m["review"]["commit"] == "abc1234",
        "{m}"
    );
    let (ok, n) = call(
        "add_note",
        json!({"name": "Review report R1", "category": "review report", "text": "Findings…", "record": "R9"}),
    )
    .await;
    assert!(ok, "{n}");
    assert!(!call("add_note", json!({"name": "x", "category": "x", "text": "x", "record": "NOPE"})).await.0);
    let overview = owner_page(&s, &cookie, "/p/vinculum/plan/M4?step=R9").await;
    let step = owner_page(&s, &cookie, &step_bar_link(&overview)).await;
    assert!(step.contains("Reports and notes") && step.contains("Review report R1"), "{step}");
    assert!(call("conclude_step", json!({"step": "R9", "summary": "Reviewed."})).await.0);
    assert!(call("update_milestone", json!({"milestone": "M4", "state": "active"})).await.0);
    let (_, h) =
        call("hello", json!({"session": "V", "machine": "m", "model": "o", "effort": "h", "brief": true})).await;
    let w = h["where"].as_array().unwrap();
    assert!(
        w.iter().any(|x| x["milestone"] == "M4" && x["hint"].as_str().unwrap().contains("every step is concluded")),
        "{h}"
    );
}

#[tokio::test]
async fn screenshots_are_uploaded_shown_and_linked() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    assert!(mcp_tool(&s, &token, &sess, "add_milestone", json!({"name": "M4", "title": "Keybinds"})).await.0);
    let (ok, t) =
        mcp_tool(&s, &token, &sess, "add_task", json!({"title": "Sidebar", "milestone": "M4", "estimate": "1 h"}))
            .await;
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
    // Its History, as in every record's dialog: no box, the newest change, Show all at the bottom
    // (the owner, 2026-11-25, D-268).
    assert!(page.contains(r#"<section class="changes changes-fold" aria-labelledby="changes-h-S-1">"#), "{page}");
    assert!(page.contains(r#"<span class="changes-when-folded">Show all "#) && !page.contains("rhist"), "{page}");
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
    assert!(page.contains(r#"<span class="chip warn">proposed</span>"#), "{page}");
    assert_eq!(
        up("name=x&category=mockup&design_url=javascript:alert(1)", png.clone()).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(up("name=x&category=mockup&state=done", png.clone()).await.status(), StatusCode::BAD_REQUEST);
    assert!(
        up("name=Sidebar%20B&category=mockup&design_url=https%3A%2F%2Fclaude.ai%2Fartifact%2Fabc", png.clone())
            .await
            .status()
            .is_success()
    );
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
    assert!(get("/p/vinculum/settings").await.contains("Restore the project"));
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
    for tab in ["", "/plan", "/plan/none", "?tab=prs", "?tab=builds", "?tab=questions", "?tab=screenshots"] {
        let res = s.http.get(s.url(&format!("/p/lumen-demo{tab}"))).header(COOKIE, &cookie).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{tab}");
    }
    // Decisions is part of Questions (Q-6, D-250): its old links open Questions.
    let old = s.http.get(s.url("/p/lumen-demo?tab=decisions")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(old.headers()[LOCATION], "/p/lumen-demo?tab=questions");
    // The Tasks tab went (D-218): its old links open Plan, and so do the overview's old board links.
    let old = s.http.get(s.url("/p/lumen-demo?tab=tasks")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(old.headers()[LOCATION], "/p/lumen-demo/plan");
    let old = s.http.get(s.url("/p/lumen-demo?ms=M3&step=K2")).header(COOKIE, &cookie).send().await.unwrap();
    assert_eq!(old.headers()[LOCATION], "/p/lumen-demo/plan/M3?step=K2");
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
    assert_eq!(save(vec![]).await, "/p/vinculum/settings?settings=saved");
    let p = s.state.db.call(|c| crate::projects::require(c, "vinculum")).await.unwrap();
    assert_eq!((p.name.as_str(), p.repo.as_str()), ("Lituus", "giosuel/vinculum"));
    // Stack and rules live in the project document now; the form doesn't touch them.
    assert!(p.stack.is_empty() && p.pm_rules.is_empty());
    assert_eq!(p.ports, ["21300 demo"]);
    assert_eq!(save(vec![("name", " ")]).await, "/p/vinculum/settings?settings=name");
    assert_eq!(save(vec![("repo", "not a repo")]).await, "/p/vinculum/settings?settings=repo");
    let page = s.http.get(s.url("/p/vinculum/settings?settings=repo")).header(COOKIE, &cookie).send().await.unwrap();
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
async fn jobs_run_on_a_builder_and_bring_back_what_was_asked() {
    use std::os::unix::fs::PermissionsExt;
    let s = TestServer::start().await;
    crate::remote::spawn(s.state.clone());
    let agent = s.token(TokenKind::Project, Some("imaginarium")).await;
    let other = s.token(TokenKind::Project, Some("vinculum")).await;
    // A real builder, in this process, in a home of its own; systemd-run and systemctl there just
    // run the command / do nothing. Its name is this test's alone: the hub is the process's.
    let name = "hq-jobs";
    let home = s.state.config.data_dir().join("hq-home");
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let write = |file: &str, text: &str| {
        let p = bin.join(file);
        std::fs::write(&p, text).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    write("systemd-run", "#!/bin/sh\nwhile [ \"$1\" != \"--\" ]; do shift; done\nshift\nexec \"$@\"\n");
    write("systemctl", "#!/bin/sh\nexit 0\n");
    write("nvidia-smi", "#!/bin/sh\nexit 1\n");
    let secret =
        s.db.call(move |c| {
            let tx = c.transaction()?;
            let b = crate::config::BuildServer {
                name: name.into(),
                min_threads: 0.0,
                min_memory_gb: 0.0,
                reserve_memory_gb: 0.0,
                ..Default::default()
            };
            crate::remote::add_daemon(&tx, &b)?;
            let (_, secret) = crate::auth::create_token(&tx, TokenKind::Builder, name, None)?;
            tx.commit()?;
            Ok(secret)
        })
        .await
        .unwrap();
    crate::remote::reload(&s.state).await;
    let token_file = home.join("builder.token");
    std::fs::write(&token_file, &secret).unwrap();
    let cfg = crate::builder::Config {
        url: s.base.clone(),
        name: name.into(),
        token_file: token_file.display().to_string(),
        home: Some(home.clone()),
    };
    let builder = tokio::spawn(crate::builder::run(cfg));
    for _ in 0..100 {
        if crate::builders::hub().is_online(name) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(crate::builders::hub().is_online(name), "the builder connected");

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
    std::fs::create_dir_all(work.join("crates/gone/src")).unwrap();
    std::fs::write(work.join("crates/gone/src/lib.rs"), "// a crate deleted later\n").unwrap();
    std::fs::create_dir_all(work.join("target")).unwrap();
    std::fs::write(work.join("target/junk"), "never sent").unwrap();

    let cmd = "cat ../a.txt ../new.txt; git describe --tags; echo 'warning: careful'; ls ../target 2>&1 | head -1; mkdir -p ../../target/release && echo built > ../../target/release/app";
    // What flow remote does: upload the snapshot (only what the server lacks), then start.
    let start = |dir: std::path::PathBuf, token: String| {
        let s = &s;
        async move {
            let (base, t2) = (s.base.clone(), token.clone());
            let d2 = dir.clone();
            let (top, sub, up) = tokio::task::spawn_blocking(move || {
                let client = crate::cli::Client::with_token(&base, &t2);
                let (top, sub) = crate::snapshot::top_of(&d2).unwrap();
                let up = match crate::snapshot::target(&client, None) {
                    Some((_, true, base)) => Some(crate::snapshot::upload(&client, &top, &base).unwrap()),
                    _ => None,
                };
                (top, sub, up)
            })
            .await
            .unwrap();
            s.http
                .post(s.url("/api/remote/jobs"))
                .bearer_auth(token)
                .json(&json!({"name": "Snapshot check", "dir": top, "subdir": sub, "command": ["sh", "-c", cmd], "fetch": ["target/release/app"],
                    "manifest": up.as_ref().map(|u| u.manifest.clone()).unwrap_or_default(),
                    "bundle": up.as_ref().map(|u| u.bundle.clone()).unwrap_or_default()}))
                .send()
                .await
                .unwrap()
        }
    };
    // A project without a build server picked is told which builders are online (not "offline").
    let res = start(work.join("crate"), agent.clone()).await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["none_picked"], true, "{v}");
    assert!(v["online"].as_array().unwrap().iter().any(|n| n == name), "{v}");
    let hint =
        s.db.call(|c| crate::remote::build_hint(c, crate::projects::get(c, "imaginarium")?.unwrap().id)).await.unwrap();
    assert!(hint["how"].as_str().unwrap().contains("--server"), "{hint}");
    s.db.call(|c| {
        let p = crate::projects::get(c, "imaginarium")?.unwrap();
        crate::remote::set_project_server(c, p.id, name)
    })
    .await
    .unwrap();
    // Every job needs a name, its title (the owner, 2026-11-25, D-275).
    let unnamed = s
        .http
        .post(s.url("/api/remote/jobs"))
        .bearer_auth(&agent)
        .json(&json!({"dir": "/x", "command": ["true"], "setup": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(unnamed.status(), StatusCode::BAD_REQUEST);
    assert!(unnamed.text().await.unwrap().contains("give the job a name"));
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
    // Its page (D-275): its name as the title, the command among its facts, the output numbered in
    // the code view a note's log uses, a warning in its tone.
    let cookie = s.owner_cookie().await;
    let page = owner_page(&s, &cookie, &format!("/machines/jobs/{id}")).await;
    assert!(page.contains(r#"<h1 class="rec-title note-title">Snapshot check</h1>"#), "{page}");
    assert!(
        page.contains("<dt>Command</dt>") && page.contains(r#"<div class="codeview cv-scroll" id="job-log""#),
        "{page}"
    );
    assert!(page.contains(r#"<span class="cv-t lv-warn">warning: careful</span>"#), "{page}");
    // Under its machine (D-277); what points somewhere is a chip (D-279).
    assert!(
        page.contains(&format!(
            r#"<li><a href="/machines" title="Machines">Machines</a></li><li><a href="/machines/{name}" title="{name}">{name}</a></li><li><span aria-current="page" title="Job #{id}">Job #{id}</span></li>"#
        )),
        "{page}"
    );
    assert!(
        page.contains(r#"<dt>Project</dt><dd><a class="chip link" href="/p/imaginarium"><svg class="i""#),
        "the project it belongs to: {page}"
    );
    assert!(page.contains(r#"<dt>Started by</dt><dd><span class="chip"><svg class="i""#), "{page}");
    assert!(page.contains(r#"s project agent</span></span></dd>"#), "{page}");
    assert!(
        page.contains(&format!(r#"<dt>Server</dt><dd><a class="chip link" href="/machines/{name}"><svg class="i""#)),
        "{page}"
    );
    assert!(page.contains(r#"<span class="cv-n">1</span>"#), "{page}");
    // flow remote fetches what came back.
    let job = s.db.call(move |c| crate::remote::get(c, id)).await.unwrap().unwrap();
    let (base, t2, w2) = (s.base.clone(), agent.clone(), work.clone());
    tokio::task::spawn_blocking(move || {
        let client = crate::cli::Client::with_token(&base, &t2);
        crate::snapshot::fetch_results(&client, id, &w2, &job.fetch, &job.artifacts).unwrap()
    })
    .await
    .unwrap();
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
    // And a file deleted here goes there too, with folders it leaves empty.
    std::fs::remove_file(work.join("new.txt")).unwrap();
    std::fs::remove_dir_all(work.join("crates")).unwrap();
    assert!(there.join("crates/gone/src/lib.rs").exists());
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
            assert!(!there.join("crates").exists(), "no empty folders left behind");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // Offline: refused at once, so the agent builds here.
    builder.abort();
    for _ in 0..100 {
        if !crate::builders::hub().is_online(name) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let res = start(work.clone(), agent.clone()).await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// Machines (D-277): an overview without job tables, a card per build machine; a page per
/// machine with its history, filtered and paged; jobs under /machines; the old addresses lead
/// there; pausing is an icon button that comes back to the page it was pressed on (D-278).
#[tokio::test]
async fn machines_are_an_overview_and_a_page_each() {
    let s = TestServer::start().await;
    let cookie = s.owner_cookie().await;
    s.token(TokenKind::Project, Some("vinculum")).await;
    // The jobs go back 29 minutes: just after midnight some would be yesterday's, and the overview
    // counts today's. Start them half an hour into the day at the earliest.
    let now = crate::util::now_ms().max(crate::tz::Tz::default().day_start(crate::util::now_ms()) + 31 * 60_000);
    s.db.call(move |c| {
        for name in ["hq", "box-2"] {
            crate::remote::add_daemon(c, &crate::config::BuildServer { name: name.into(), ..Default::default() })?;
        }
        let pid = crate::projects::get(c, "vinculum")?.unwrap().id;
        // 25 jobs on hq, the oldest first: every fifth failed, the newest still waiting.
        for i in 1..=25i64 {
            let state = if i == 25 {
                "queued"
            } else if i % 5 == 0 {
                "failed"
            } else {
                "done"
            };
            c.execute(
                "INSERT INTO remote_jobs (server, project_id, actor, dir, command, state, reason, created_at, started_at, finished_at, name)
                 VALUES ('hq', ?1, 'vinculum', '/w', '[\"cargo\",\"test\"]', ?2, ?3, ?4, ?4, ?5, ?6)",
                rusqlite::params![
                    pid,
                    state,
                    if state == "failed" { "exit code 101" } else { "" },
                    now - (30 - i) * 60_000,
                    if state == "queued" { None } else { Some(now - (30 - i) * 60_000 + 30_000) },
                    format!("Job number {i}")
                ],
            )?;
        }
        Ok(())
    })
    .await
    .unwrap();
    crate::remote::reload(&s.state).await;
    let ids: Vec<i64> =
        s.db.call(|c| {
            let mut st = c.prepare("SELECT id FROM remote_jobs ORDER BY id")?;
            let r = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            Ok(r)
        })
        .await
        .unwrap();
    let get = |path: &str| {
        let (http, url, cookie) = (s.http.clone(), s.url(path), cookie.clone());
        async move { http.get(url).header(COOKIE, cookie).send().await.unwrap() }
    };

    // The overview: AgentFlow's figures and a card per build machine, no job tables.
    let html = get("/machines").await.text().await.unwrap();
    assert!(html.contains("<h1>Machines</h1>") && html.contains("AgentFlow runs on "), "{html}");
    assert!(html.contains("2 build machines, 0 online") && html.contains("nothing running, 1 waiting"), "{html}");
    assert_eq!(html.matches(r#"<section class="card mcard off""#).count(), 2, "dimmed while offline: {html}");
    assert!(html.contains(r#""><a href="/machines/hq">hq</a></h2><span class="sub">offline</span>"#), "{html}");
    assert!(html.contains(r#"<a class="act ilink" href="/machines/box-2"><span>Its builds</span>"#), "{html}");
    assert!(html.contains("Today 25 jobs, 4 failed") && html.contains("No jobs today"), "{html}");
    assert!(!html.contains("<table") && !html.contains("data-sortable"), "no job tables: {html}");
    assert!(!html.contains("By project") && !html.contains("Claimed ports"), "D-283: {html}");
    // The nav says Machines, lit here.
    assert!(
        html.contains(r#"<a href="/machines" class="on" aria-current="page">"#)
            && html.contains("<span>Machines</span>"),
        "{html}"
    );
    let dash = get("/").await.text().await.unwrap();
    assert!(dash.contains(r#"<a href="/machines" aria-label="Machines" data-name="Machines">"#), "{dash}");
    assert!(!dash.contains(r#"href="/machine""#), "{dash}");
    assert!(get("/more").await.text().await.unwrap().contains(r#"<a href="/machines">"#));

    // Pausing: an icon button, back to the page it was pressed on (only a Machines page).
    let post = |path: &str, next: &str| {
        let (http, url, cookie, base) = (s.http.clone(), s.url(path), cookie.clone(), s.base.clone());
        let form = vec![("next", next.to_owned())];
        async move {
            let res = http.post(url).header(COOKIE, cookie).header(ORIGIN, base).form(&form).send().await.unwrap();
            assert_eq!(res.status(), StatusCode::SEE_OTHER);
            res.headers()[LOCATION].to_str().unwrap().to_owned()
        }
    };
    assert_eq!(post("/machines/hq/pause", "/machines/hq?show=failed").await, "/machines/hq?show=failed");
    let html = get("/machines").await.text().await.unwrap();
    assert!(
        html.contains(
            r#"<button class="ibtn" type="submit" title="Resume builds on hq" aria-label="Resume builds on hq">"#
        ) && html.contains("Builds are paused"),
        "{html}"
    );
    assert_eq!(post("/machines/hq/resume", "https://evil.example/").await, "/machines", "no open redirects");
    // A page open from before posts to the old address.
    assert_eq!(post("/machine/servers/hq/pause", "").await, "/machines");
    assert_eq!(post("/machine/servers/hq/resume", "").await, "/machines");
    assert_eq!(get("/machines/nowhere/pause").await.status(), StatusCode::METHOD_NOT_ALLOWED);

    // A machine's page: its path, its history 20 a page, newest first, with the pager.
    let html = get("/machines/hq").await.text().await.unwrap();
    assert!(
        html.contains(r#"<li><a href="/machines" title="Machines">Machines</a></li><li><span aria-current="page" title="hq">hq</span></li>"#)
            && html.contains(r#"<h1 class="rec-title note-title">hq</h1>"#),
        "{html}"
    );
    assert!(
        html.contains("25 jobs since ")
            && html.contains(r#"<h2 id="hist-h">Build history</h2><span class="sub">25 jobs</span>"#),
        "{html}"
    );
    assert!(html.contains(r#"<table class="table jobs" data-sortable>"#), "the table component: {html}");
    assert_eq!(html.matches(r#"<tr><th scope="row" data-v=""#).count(), 20, "{html}");
    assert!(html.contains(&format!(r#"<a href="/machines/jobs/{}">#{}</a>"#, ids[24], ids[24])), "the newest: {html}");
    assert!(!html.contains(&format!(r#"<a href="/machines/jobs/{}">#{}</a></th>"#, ids[4], ids[4])), "{html}");
    assert!(
        html.contains(r#"<span class="sub pager-range">1–20 of 25</span><span class="sub mono">page 1 of 2</span>"#)
            && html.contains(
                r##"<a class="ibtn" href="/machines/hq?page=2#history" title="Older jobs" aria-label="Older jobs">"##
            ),
        "{html}"
    );
    // Right now: the waiting job.
    assert!(html.contains("offline · 1 waiting") && html.contains("Job number 25"), "{html}");
    assert!(html.contains(r#"<dt>Starts a job</dt><dd>while 8 threads and 16 GB are free</dd>"#), "{html}");
    assert!(html.contains(r#"href="/settings?s=builders" data-settings"#), "{html}");
    let html = get("/machines/hq?page=2").await.text().await.unwrap();
    assert_eq!(html.matches(r#"<tr><th scope="row" data-v=""#).count(), 5, "{html}");
    assert!(
        html.contains("21–25 of 25") && html.contains(r##"href="/machines/hq#history" title="Newer jobs""##),
        "{html}"
    );
    // Filters: failed, then running or waiting; a page past the end is the last one.
    let html = get("/machines/hq?show=failed&page=9").await.text().await.unwrap();
    assert_eq!(html.matches(r#"<tr><th scope="row" data-v=""#).count(), 4, "{html}");
    assert!(
        html.contains("4 failed of 25") && html.contains(r#"<span class="sub pager-range">1–4 of 4</span></nav>"#),
        "{html}"
    );
    assert!(
        html.contains(r##"<a class="on" aria-current="true" href="/machines/hq?show=failed#history">Failed</a>"##),
        "{html}"
    );
    let html = get("/machines/hq?show=running&page=x").await.text().await.unwrap();
    assert_eq!(html.matches(r#"<tr><th scope="row" data-v=""#).count(), 1, "{html}");
    assert!(html.contains(r#"<span class="chip ">waiting</span>"#), "{html}");
    let html = get("/machines/box-2").await.text().await.unwrap();
    assert!(html.contains("No jobs yet: agents build here with flow remote --server box-2."), "{html}");
    assert_eq!(get("/machines/nowhere").await.status(), StatusCode::NOT_FOUND);

    // The old addresses lead to the new ones.
    for (old, new) in [
        ("/machine", "/machines".to_owned()),
        ("/machine/now", "/machines".to_owned()),
        ("/machine/builds", "/machines".to_owned()),
        (&*format!("/machine/jobs/{}", ids[0]), format!("/machines/jobs/{}", ids[0])),
    ] {
        let res = get(old).await;
        assert!(res.status().is_redirection(), "{old}");
        assert_eq!(res.headers()[LOCATION], new.as_str(), "{old}");
    }
    let page = get(&format!("/machines/jobs/{}", ids[0])).await.text().await.unwrap();
    assert!(page.contains(r#"<li><a href="/machines/hq" title="hq">hq</a></li>"#), "{page}");
    // Its output answers at both addresses (a page open from before follows it there).
    for path in [format!("/machines/jobs/{}/output", ids[0]), format!("/machine/jobs/{}/output", ids[0])] {
        assert_eq!(get(&path).await.status(), StatusCode::OK, "{path}");
    }
    // Agents don't see the pages.
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let res = s.http.get(s.url("/machines/hq")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn builders_are_taken_over_and_picked_per_project() {
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
    // A former SSH builder: no token yet, so the card says to run flow builder add on it, and
    // flow builder add takes it over; one with a token can't be taken.
    s.db.call(|c| {
        c.execute(
            "INSERT INTO build_servers (name, ssh, key_file, root, min_threads, min_memory_gb, reserve_memory_gb, keep_days, created_at, kind)
             VALUES ('hq', 'builder@10.0.8.2', '', 'agentflow', 8, 16, 8, 14, 1, 'ssh')",
            [],
        )?;
        c.execute_batch(include_str!("../../migrations/024_no_ssh_builders.sql"))?;
        Ok(())
    })
    .await
    .unwrap();
    crate::remote::reload(&s.state).await;
    let page =
        s.http.get(s.url("/settings?s=builders")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("waiting for flow builder add") && !page.contains("builder@10.0.8.2"), "no SSH left");
    assert!(!page.contains("Add a build server"), "no SSH form");
    s.db.call(|c| {
        // "HQ" from flow builder add takes over "hq", whatever the case.
        assert_eq!(crate::remote::same_name(c, "HQ")?.as_deref(), Some("hq"));
        let b = crate::config::BuildServer { name: "hq".into(), ..Default::default() };
        crate::remote::add_daemon(c, &b)?;
        crate::auth::create_token(c, TokenKind::Builder, "hq", None)?;
        assert!(crate::remote::add_daemon(c, &b).is_err(), "taken once it has a token");
        Ok(())
    })
    .await
    .unwrap();
    // A project picks it in its Settings; the Settings card lists the project.
    let page =
        s.http.get(s.url("/p/vinculum/settings")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
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
    let page =
        s.http.get(s.url("/settings?s=builders")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains(r#"<span class="chip">Vinculum</span>"#), "the project in the builder's Used by");
    // Limits, then removal: its token goes, the project builds locally again.
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
    assert!(crate::remote::servers(&s.state).is_empty());
    assert!(!s.db.call(|c| crate::remote::has_token(c, "hq")).await.unwrap(), "its token is revoked");
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
        let (ok, r) =
            call("update_milestone", json!({"milestone": n, "state": "closed", "summary": format!("{n} done.")})).await;
        assert!(ok, "{r}");
    }
    let (ok, r) =
        call("add_tasks", json!({"milestone": "P9.3b", "tasks": [{"title": "Batch API", "state": "done"}]})).await;
    assert!(ok && r["ids"] == json!(["T-1"]), "{r}");
    let (ok, r) =
        call("add_tasks", json!({"milestone": "P9.3c", "tasks": [{"title": "Ask UI", "state": "done"}]})).await;
    assert!(ok && r["ids"] == json!(["T-2"]), "{r}");
    let (ok, n) =
        call("add_note", json!({"name": "Batch log", "category": "log", "text": "ok", "record": "P9.3b"})).await;
    assert!(ok, "{n}");
    // Fold P9.3b into P9.3: a done task with its dates, then the step archived with move_to.
    let (ok, r) = call("add_tasks", json!({"milestone": "P9.3", "tasks": [
        {"title": "P9.3b: Batches", "state": "done", "body": "P9.3b done.", "started_at": "2026-11-09", "done_at": "2026-11-09 18:00"}]})).await;
    assert!(ok, "{r}");
    assert!(!call("add_tasks", json!({"milestone": "P9.3", "tasks": [{"title": "x", "state": "todo", "estimate": "1 h", "done_at": "2026-11-09"}]})).await.0);
    assert!(!call("archive_milestone", json!({"milestone": "P9.3b", "reason": ""})).await.0, "a reason");
    let (ok, d) =
        call("archive_milestone", json!({"milestone": "P9.3b", "move_to": "P9.3", "reason": "folded into P9.3"})).await;
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
    assert!(
        ok && m["summary"] == "P9.3c done." && m["archived"]["reason"] == "not needed" && m["tasks"][0]["id"] == "T-2",
        "{m}"
    );
    // A concluded step in one call, as history.
    let (ok, m) = call(
        "add_milestone",
        json!({"name": "P9.9", "title": "Old", "step_of": "P9", "state": "closed",
        "started_at": "2026-11-01", "closed_at": "2026-11-02", "summary": "Done long ago."}),
    )
    .await;
    assert!(ok && m["state"] == "closed", "{m}");
    let (_, m) = call("get_milestone", json!({"milestone": "P9.9"})).await;
    assert!(m["closed_at"].as_str().unwrap().starts_with("2026-11-02") && m["summary"] == "Done long ago.", "{m}");
    let (ok, r) = call("restore_milestone", json!({"milestone": "P9.3c"})).await;
    assert!(ok && r["with"] == json!(["T-2"]), "{r}");
    let (_, t2) = call("get_record", json!({"id": "T-2"})).await;
    assert_eq!(t2["state"], "done", "{t2}");
    // Steps and milestones edit freely: reopened, moved to another milestone, summary rewritten.
    assert!(
        call("update_milestone", json!({"milestone": "P9.3", "summary": "Agents drive the app; batches."})).await.0
    );
    let (ok, r) = call("add_task", json!({"milestone": "P10", "title": "Later", "estimate": "1 h"})).await;
    assert!(ok, "{r}");
    assert!(call("update_task", json!({"id": "T-4", "milestone": "P9.3"})).await.0);
    assert!(call("update_milestone", json!({"milestone": "P9.3", "state": "active"})).await.0);
    assert!(call("update_milestone", json!({"milestone": "P9.3", "step_of": "P10"})).await.0);
    // Tasks archive with a reason and come back with any other state.
    assert!(!call("update_task", json!({"id": "T-4", "state": "archived"})).await.0, "a reason");
    let (ok, r) = call("update_task", json!({"id": "T-4", "state": "archived", "note": "a duplicate of T-1"})).await;
    assert!(ok, "{r}");
    let tasks = s
        .http
        .get(s.url("/p/vinculum?tab=tasks"))
        .header(COOKIE, s.owner_cookie().await)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!tasks.contains("Later"), "{tasks}");
    assert!(call("update_task", json!({"id": "T-4", "state": "todo"})).await.0);
    // A milestone takes its steps, tasks and the decisions and questions scoped to it along.
    assert!(call("update_milestone", json!({"milestone": "P10", "state": "active"})).await.0);
    let (ok, d) =
        call("record_decision", json!({"title": "Keep it small", "owner_words": "small", "source": "owner"})).await;
    assert!(ok, "{d}");
    let (ok, d) = call(
        "record_decision",
        json!({"title": "Always ask first", "owner_words": "ask", "source": "owner", "scope": "project"}),
    )
    .await;
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
        .call(|c| {
            Ok(c.query_row("SELECT group_concat(summary, ' | ') FROM events WHERE kind LIKE 'milestone.%'", [], |r| {
                r.get(0)
            })?)
        })
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
    assert!(
        call("add_task", json!({"milestone": "P9.1", "title": "Work", "estimate": "1 h", "state": "doing"})).await.0
    );
    let (_, q) = call("ask_question", json!({"title": "Which?"})).await;
    assert_eq!(q["scope"], "P9", "{q}");
    // A step names its milestone; "project"; a planned milestone.
    let (_, q) = call("ask_question", json!({"title": "Step?", "scope": "M9.1"})).await;
    assert_eq!(q["scope"], "M9");
    let (_, q) = call("ask_question", json!({"title": "Everywhere?", "scope": "project", "related": ["D-9"]})).await;
    assert!(q.as_str().is_some_and(|e| e.contains("D-9")), "related must be decisions: {q}");
    let (ok, d) =
        call("record_decision", json!({"title": "Old rule", "owner_words": "old", "source": "owner", "scope": "M8"}))
            .await;
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
    // The agent moves a question to another scope, not the owner (2026-11-25, D-288): its page
    // only says where the decision goes.
    let (ok, r) = call("update_question", json!({"id": "Q-3", "scope": "M9", "reason": "about M9"})).await;
    assert!(ok && r["scope"] == "M9", "{r}");
    let cookie = s.owner_cookie().await;
    let page = s.http.get(s.url("/p/vinculum/Q-3")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("Scope: M9 · ") && !page.contains(r#"name="scope""#), "{page}");
    let res = s
        .http
        .post(s.url("/p/vinculum/Q-3/scope"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("scope", "project")])
        .send()
        .await
        .unwrap();
    assert!(!res.status().is_success() && !res.status().is_redirection(), "{}", res.status());
    let res = s
        .http
        .post(s.url("/p/vinculum/Q-3/answer"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("words", "yes, everywhere")])
        .send()
        .await
        .unwrap();
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
    let (ok, d) =
        call("record_decision", json!({"title": "New rule", "owner_words": "new", "source": "owner", "scope": "M8"}))
            .await;
    assert!(ok, "{d}");
    let new_id = d["id"].as_str().unwrap().to_owned();
    let (ok, r) =
        call("update_decision", json!({"ids": ["D-1", new_id.clone()], "scope": "M9", "reason": "about M9"})).await;
    assert!(ok && r["decisions"].as_array().unwrap().len() == 2, "{r}");
    let (ok, r) =
        call("update_decision", json!({"id": "D-1", "replaced_by": new_id.clone(), "reason": "the owner changed it"}))
            .await;
    assert!(ok && r["state"] == "replaced", "{r}");
    let (ok, r) =
        call("update_decision", json!({"id": new_id.clone(), "scope": "project", "reason": "holds everywhere"})).await;
    assert!(ok && r["scope"] == "project", "{r}");
    // Default reads: project-wide and open milestones' decisions; closed and replaced need a flag.
    let (ok, d) =
        call("record_decision", json!({"title": "M8 thing", "owner_words": "m8", "source": "owner", "scope": "M8"}))
            .await;
    assert!(ok, "{d}");
    assert!(call("update_milestone", json!({"milestone": "M8", "state": "closed"})).await.0);
    let (_, found) = call("find_records", json!({"kind": "decision"})).await;
    let ids: Vec<String> =
        found["records"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_owned()).collect();
    let m8_thing = d["id"].as_str().unwrap().to_owned();
    assert!(!ids.contains(&"vinculum/D-1".to_owned()) && !ids.contains(&m8_thing), "{found}");
    let (_, found) = call("find_records", json!({"kind": "decision", "include_closed": true})).await;
    assert_eq!(found["count"], ids.len() + 2, "{found}");
    // Pages: count is the page, total all of them; compact leaves the quotes out.
    let (_, page) = call(
        "find_records",
        json!({"kind": "decision", "include_closed": true, "limit": 1, "offset": 1, "compact": true}),
    )
    .await;
    assert!(page["count"] == 1 && page["total"] == ids.len() + 2 && page["more"] == true, "{page}");
    assert!(page["records"][0].get("owner_words").is_none() && page["records"][0]["scope"].is_string(), "{page}");
    // The Decisions tab (M16.1): in force by default; a replaced decision folds under the one that
    // replaced it, a closed milestone's wait under Scope; the filters bring the rest in.
    let short = |id: &str| id.rsplit('/').next().unwrap().to_owned();
    let tab = owner_page(&s, &cookie, "/p/vinculum?tab=questions").await;
    assert!(tab.contains("New rule") && !tab.contains("M8 thing"), "{tab}");
    assert!(tab.contains(r#"class="dec-old""#) && tab.contains(&format!("· replaced by {}", short(&new_id))), "{tab}");
    assert!(tab.contains("Closed milestones") && tab.contains("1 replaced hidden: State"), "{tab}");
    let m8 = owner_page(&s, &cookie, "/p/vinculum?tab=questions&dscope=M8").await;
    assert!(m8.contains("M8 thing") && !m8.contains("New rule"), "{m8}");
    let replaced = owner_page(&s, &cookie, "/p/vinculum?tab=questions&dstate=replaced&dscope=M9").await;
    assert!(replaced.contains(&format!("replaced by {}", short(&new_id))) && !replaced.contains(r#"class="dec-old""#));
    let found = owner_page(&s, &cookie, "/p/vinculum?tab=questions&dq=new+rule").await;
    assert!(found.contains("<mark>New</mark> <mark>rule</mark>") && found.contains(" found</span>"), "{found}");
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
    assert!(
        call("hello", json!({"session": "Vinculum", "machine": "giosuel.ch", "model": "Opus", "effort": "high"}))
            .await
            .0
    );
    assert!(call("edit_project_doc", json!({"section": "How to work on it", "text": "- DCO sign-off"})).await.0);
    assert!(
        call(
            "add_milestone",
            json!({"name": "M1", "title": "Done one", "description": "long text", "state": "closed"})
        )
        .await
        .0
    );
    assert!(
        call("add_milestone", json!({"name": "M2", "title": "Now", "description": "what M2 is", "state": "active"}))
            .await
            .0
    );
    let (ok, q) = call("ask_question", json!({"title": "Which?"})).await;
    assert!(ok, "{q}");
    let cookie = s.owner_cookie().await;
    let res = s
        .http
        .post(s.url("/p/vinculum/Q-1/answer"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .form(&[("words", "this one")])
        .send()
        .await
        .unwrap();
    assert!(res.status().is_redirection());

    let res = s
        .http
        .post(s.url("/api/hello"))
        .bearer_auth(&token)
        .json(&json!({"source": "compact", "model": "Opus 5.5"}))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success(), "{}", res.status());
    let r: Value = res.json().await.unwrap();
    assert!(r["doc"]["text"].as_str().unwrap().contains("DCO sign-off"), "{r}");
    assert!(r["news"].to_string().contains("Owner answered Q-1"), "{r}");
    let text = crate::cli::hello_text(&r, "compact", crate::cli::HelloPart::All);
    assert!(text.starts_with("AgentFlow hello after your context was summarized"), "{text}");
    assert!(text.contains("DCO sign-off") && text.contains("Owner answered Q-1"), "{text}");
    assert!(
        text.contains("- M1 Done one [closed]") && !text.contains("long text") && text.contains("what M2 is"),
        "{text}"
    );
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
    big["recent_events"] = (0..200)
        .map(|i| json!({"at": "2026-11-12T10:00:00Z", "what": format!("event {i} {}", "x".repeat(80)), "by": "x"}))
        .collect();
    let doc = crate::cli::hello_text(&big, "startup", crate::cli::HelloPart::Doc);
    assert!(doc.chars().count() <= crate::cli::HOOK_LIMIT && doc.contains("the document goes on"), "{}", doc.len());
    let rest = crate::cli::hello_text(&big, "startup", crate::cli::HelloPart::NoDoc);
    assert!(rest.chars().count() <= crate::cli::HOOK_LIMIT, "{}", rest.len());
    assert!(
        !rest.contains("a rule")
            && rest.contains("=== Where you are")
            && rest.contains("Owner answered Q-1")
            && rest.contains("what M2 is")
            && rest.contains("cut to fit"),
        "{rest}"
    );
    // Not for the owner.
    let res = s
        .http
        .post(s.url("/api/hello"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, &s.base)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert!(!res.status().is_success());
}

/// Every tool's schema is written out in full: no $ref for a client to follow (Imaginarium's
/// didn't see update_milestone's review).
#[tokio::test]
async fn tool_schemas_have_no_refs() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let (_, r) =
        mcp_call(&s, &token, Some(&sess), json!({"jsonrpc": "2.0", "id": 9, "method": "tools/list", "params": {}}))
            .await;
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
        async move {
            s.http
                .post(s.url("/api/setup/redeem"))
                .json(&json!({"code": code, "machine": "giosuel@mothman"}))
                .send()
                .await
                .unwrap()
        }
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
    let res = s
        .http
        .post(s.url("/api/setup/done"))
        .bearer_auth(&token)
        .json(&json!({"folder": "relative", "machine": "m"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let page = s.http.get(s.url("/p/vinculum")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(
        page.contains("/home/giosuel/vinculum")
            && page.contains("mothman")
            && page.contains("Set up on another machine"),
        "{page}"
    );
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
    let tokens: i64 =
        s.db.call(move |c| Ok(c.query_row("SELECT count(*) FROM tokens WHERE project_id = ?1", [p.id], |r| r.get(0))?))
            .await
            .unwrap();
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
        tokio::task::spawn_blocking(move || {
            crate::sync::send(&crate::cli::Client::with_token(&base, &tok), &[f]).unwrap()
        })
    };
    assert_eq!(send(token.clone(), file.clone(), s.base.clone()).await.unwrap(), 2);
    assert_eq!(send(token.clone(), file.clone(), s.base.clone()).await.unwrap(), 0, "nothing twice");
    // Another project's token keeps its own offsets: its upload counts nothing new either.
    assert_eq!(send(other, file.clone(), s.base.clone()).await.unwrap(), 0);
    let mut f = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
    std::io::Write::write_all(&mut f, line("m3", "2026-11-13T01:02:00Z").as_bytes()).unwrap();
    assert_eq!(send(token.clone(), file.clone(), s.base.clone()).await.unwrap(), 1);
    let rows: Vec<(String, Option<String>)> =
        s.db.call(|c| {
            let mut st = c.prepare(
                "SELECT u.message_id, p.slug FROM usage u LEFT JOIN projects p ON p.id = u.project_id ORDER BY 1",
            )?;
            let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|(_, p)| p.as_deref() == Some("vinculum")), "{rows:?}");
    // Only projects' tokens sync.
    let device = s.token(TokenKind::Device, None).await;
    let res =
        s.http.post(s.url("/api/sync/state")).bearer_auth(&device).json(&json!({"file": "m:/x"})).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

/// A builder that connects itself gets its token after the owner approves it; the token opens the
/// builder's endpoints and nothing else, and agents' tokens can't use those.
#[tokio::test]
async fn a_builders_token_is_only_for_building() {
    let s = TestServer::start().await;
    let device = s.token(TokenKind::Device, None).await;
    let cookie = s.owner_cookie().await;
    let asked: Value = s
        .http
        .post(s.url("/api/approvals"))
        .bearer_auth(&device)
        .json(&json!({"action": "builder_create", "args": {"name": "testbox", "threads": 16, "memory_gb": 64}, "machine": "builder@testbox"}))
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
        .form(&[("decision", "approve")])
        .send()
        .await
        .unwrap();
    let got: Value = s
        .http
        .post(s.url("/api/approvals/poll"))
        .json(&json!({"poll": asked["poll"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = got["result"]["token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("af_bld_"), "{got}");
    assert_eq!(got["result"]["min_threads"], 4.0, "a quarter of its threads");
    let (kind,): (String,) = s
        .db
        .call(|c| Ok(c.query_row("SELECT kind FROM build_servers WHERE name = 'testbox'", [], |r| Ok((r.get(0)?,)))?))
        .await
        .unwrap();
    assert_eq!(kind, "daemon");
    for path in ["/api/status", "/api/projects", "/mcp"] {
        let res = s.http.get(s.url(path)).bearer_auth(&token).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{path}");
    }
    let res =
        s.http.post(s.url("/api/builders/blobs")).bearer_auth(&token).json(&json!({"shas": []})).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "its own endpoint");
    let agent = s.token(TokenKind::Project, Some("vinculum")).await;
    let res =
        s.http.post(s.url("/api/builders/blobs")).bearer_auth(&agent).json(&json!({"shas": []})).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "not for agents");
    // Agents upload snapshots; the store takes only what matches its hash.
    let data = b"fn main() {}\n".to_vec();
    let sha = crate::blobs::sha_of(&data);
    let missing: Value = s
        .http
        .post(s.url("/api/remote/blobs/missing"))
        .bearer_auth(&agent)
        .json(&json!({"shas": [sha]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(missing["missing"][0], sha.as_str());
    let mut pack = Vec::new();
    crate::blobs::pack_bytes(&[(sha.clone(), data)], &mut pack).unwrap();
    let res = s.http.post(s.url("/api/remote/blobs")).bearer_auth(&agent).body(pack).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let mut bad = Vec::new();
    crate::blobs::pack_bytes(&[(sha, b"other".to_vec())], &mut bad).unwrap();
    let res = s.http.post(s.url("/api/remote/blobs")).bearer_auth(&agent).body(bad).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "known already: kept as it was");

    // flow builder list / edit: a connected machine sees it and changes its name and limits; the
    // token moves with the name.
    let list: Value =
        s.http.get(s.url("/api/builders")).bearer_auth(&device).send().await.unwrap().json().await.unwrap();
    assert!(
        list["builders"].as_array().unwrap().iter().any(|b| b["name"] == "testbox" && b["kind"] == "daemon"),
        "{list}"
    );
    let res = s.http.get(s.url("/api/builders")).bearer_auth(&agent).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "not for agents");
    let res = s
        .http
        .patch(s.url("/api/builders/edit/testbox"))
        .bearer_auth(&device)
        .json(&json!({"name": "box-2", "min_threads": 6.0, "keep_days": 7}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res =
        s.http.post(s.url("/api/builders/blobs")).bearer_auth(&token).json(&json!({"shas": []})).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "the token works under the new name");
    let (n, t, k): (String, f64, i64) = s
        .db
        .call(|c| Ok(c.query_row("SELECT b.name, b.min_threads, b.keep_days FROM build_servers b JOIN tokens t ON t.name = b.name AND t.kind = 'builder'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?))
        .await
        .unwrap();
    assert_eq!((n.as_str(), t, k), ("box-2", 6.0, 7));
    // flow builder remove, on the builder: it leaves and its token stops working.
    let res = s.http.delete(s.url("/api/builders/self")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res =
        s.http.post(s.url("/api/builders/blobs")).bearer_auth(&token).json(&json!({"shas": []})).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

/// The UI library (M15): pages load its stylesheet first and carry the icon sprite scripts draw
/// from, and the button and icon-button kinds it replaced are gone from templates and scripts.
/// The tokens are the only place for a colour or a size (M15.5, T-120; the owner, 2026-11-23,
/// D-245: "make sure that we reuse all these elements instead of writing new ones from scratch").
/// Outside ui.css's token block there's no hex colour and no font size in px, and an icon's size
/// is one of the four icon tokens. A new one-off value fails here; it goes into the tokens, or
/// uses one that's there.
#[test]
fn colours_and_sizes_come_from_the_tokens() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static");
    let strip = |css: &str| {
        let mut out = String::new();
        let mut rest = css;
        while let Some(i) = rest.find("/*") {
            out.push_str(&rest[..i]);
            rest = rest[i..].find("*/").map_or("", |j| &rest[i + j + 2..]);
        }
        out + rest
    };
    let ui = std::fs::read_to_string(root.join("ui.css")).unwrap();
    let tokens_end =
        ui.find(":root").and_then(|i| ui[i..].find('}').map(|j| i + j)).expect("ui.css starts with its tokens");
    for (name, css) in [
        ("ui.css", strip(&ui[tokens_end..])),
        ("app.css", strip(&std::fs::read_to_string(root.join("app.css")).unwrap())),
    ] {
        for (n, line) in css.lines().enumerate() {
            let hex = line.match_indices('#').any(|(i, _)| {
                let h: String = line[i + 1..].chars().take_while(|c| c.is_ascii_hexdigit()).collect();
                matches!(h.len(), 3 | 4 | 6 | 8)
                    && !line[i + 1 + h.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-')
            });
            assert!(!hex, "{name}, line {} (comments stripped): a hex colour outside the tokens: {line}", n + 1);
            let px_font = line.split("font-size:").skip(1).any(|v| {
                let v = v.trim_start();
                v.starts_with(|c: char| c.is_ascii_digit())
                    && v.split([';', '}']).next().unwrap_or("").trim().ends_with("px")
            });
            assert!(!px_font, "{name}, line {}: a font size in px outside the tokens: {line}", n + 1);
            // An icon's size: a rule whose selector names .i sets width and height with the tokens.
            if let Some((sel, decls)) = line.split_once('{') {
                let icon = sel
                    .split(|c: char| c == ',' || c == '>' || c.is_whitespace())
                    .any(|t| t == ".i" || t.starts_with(".i:") || t.starts_with(".i."));
                for d in decls.split([';', '}']) {
                    let Some((k, v)) = d.split_once(':') else { continue };
                    let (k, v) = (k.trim(), v.trim());
                    let px = v.starts_with(|c: char| c.is_ascii_digit()) && v.ends_with("px");
                    assert!(
                        !(icon && (k == "width" || k == "height") && px),
                        "{name}, line {}: an icon size outside --ic-2xs/--ic-xs/--ic-sm/--ic/--ic-lg: {line}",
                        n + 1
                    );
                }
            }
        }
    }
}

#[test]
fn the_ui_library_replaced_the_old_buttons() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let base = std::fs::read_to_string(root.join("templates/base.html")).unwrap();
    let ui = base.find("/static/ui.css").expect("ui.css linked");
    assert!(ui < base.find("/static/app.css").unwrap(), "the library first, page layouts after");
    assert!(base.contains("icons::sprite()"));
    // A page's stylesheet lays pages out; it never redefines a library component (the library's
    // `.bare` once met the sign-in page's `.bare` and grew the copy button by 48 px).
    let app = std::fs::read_to_string(root.join("static/app.css")).unwrap();
    let ui = std::fs::read_to_string(root.join("static/ui.css")).unwrap();
    let class_of = |l: &str| -> Option<String> {
        let rest = l.trim_start().strip_prefix('.')?;
        Some(rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).next()?.to_owned())
    };
    // Every class a library rule names, not only its first: `.field .opt` met the answer cards' `.opt`.
    let roots: std::collections::BTreeSet<String> = ui
        .lines()
        .filter(|l| l.contains('{') && !l.trim_start().starts_with(['/', '@', '-']))
        .flat_map(|l| {
            let sel = l.split('{').next().unwrap_or("");
            sel.split('.')
                .skip(1)
                .map(|c| {
                    c.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
                        .next()
                        .unwrap_or("")
                        .to_owned()
                })
                .collect::<Vec<_>>()
        })
        .filter(|c| !c.is_empty())
        .collect();
    for line in app.lines() {
        if let Some(class) = class_of(line) {
            assert!(!roots.contains(&class), "app.css redefines the library's .{class}: {line}");
        }
    }
    // Every card's header is the one bar (the owner, 2026-11-25, D-276): no card opens with a title
    // inside its padding or an uppercase label instead.
    for f in std::fs::read_dir("templates").unwrap() {
        let path = f.unwrap().path();
        let t = std::fs::read_to_string(&path).unwrap();
        for open in ["<section class=\"card pad", "<div class=\"card pad", "<details class=\"card pad", "<form"] {
            for (i, _) in t.match_indices(open) {
                let tag_end = i + t[i..].find('>').unwrap() + 1;
                if open == "<form" && !t[i..tag_end].contains("class=\"card pad") {
                    continue;
                }
                let next = t[tag_end..].trim_start();
                // A heading only screen readers see (class vh) isn't a header.
                let bad =
                    ["<div class=\"sec", "<h2", "<h3", "<summary class=\"label"].iter().any(|h| next.starts_with(h))
                        && !next[..next.find('>').unwrap_or(0)].contains("class=\"vh\"");
                assert!(
                    !bad,
                    "{}: a card with its own kind of header; use .card-head: {}",
                    path.display(),
                    &t[i..tag_end]
                );
            }
        }
    }
    let gone = [
        "ghost",
        "lsearch",
        "\"mtag",
        "\"mstate",
        "class=\"imp",
        "class=\"plat",
        "class=\"lx",
        "addtag",
        "sevdot",
        "chip subagent",
        "state-done",
        "code-chip",
        "next-tag",
        "class=\"ranges",
        "class=\"rng",
        "seg3",
        "cv-chip",
        "class=\"pills",
        "sb-menu",
        "edit.g.",
        "<datalist",
        "solid-danger",
        "icon-only",
        "reload-btn",
        "qp-btn",
        "pg-btn",
        "copy-btn",
        "rdlg-btn",
        "link-btn",
        "class=\"dlb",
        // One dialog (PR 8): the five shells and their scroll locks.
        "rdlg-back",
        "rdlg-body",
        "rdlg-head",
        "rdlg-on",
        "cdlg-back",
        "cdlg-btns",
        "sdlg-back",
        "sdlg-head",
        "sdlg-body",
        "pdlg-body",
        "ntf-back",
        "ntf-head",
        "no-scroll",
        // One table, one list row, one key-value list, one timeline (PR 9).
        "class=\"timing\"",
        "class=\"cmp\"",
        "class=\"grp\"",
        "class=\"item\"",
        "class=\"item ",
        "info-dl",
        "hist-ev",
        "hist-pt",
        "hist-tl",
        // Notices, empty states, numbers, meters, legends (PR 10); icon tiles and glyphs (PR 11).
        "mp-bar",
        "mp-80",
        "class=\"tbar",
        "ntf-empty",
        "lempty",
        "plan-empty",
        "ovp-pad",
        "ml-nothing",
        "tip-sw",
        "class=\"key\"",
        "class=\"ico\"",
        "meta-ico",
        "‹ Newer",
        "Older: {{ t }} ›",
        "Usage ›",
    ];
    for dir in ["templates", "static"] {
        for entry in std::fs::read_dir(root.join(dir)).unwrap() {
            let path = entry.unwrap().path();
            if !matches!(path.extension().and_then(|e| e.to_str()), Some("html" | "js" | "css")) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for old in gone {
                assert!(!text.contains(old), "{} still uses {old}", path.display());
            }
            if path.extension().is_some_and(|e| e == "js") {
                assert!(
                    !text.contains(">×<") && !text.contains(">‹<"),
                    "{}: icons come from the sprite",
                    path.display()
                );
                // Confirms are the app's dialog (afDialog), never the browser's.
                assert!(
                    !text.contains(" confirm(")
                        && !text.contains("(confirm(")
                        && !text.contains(" alert(")
                        && !text.contains(" prompt("),
                    "{}: the browser's own dialogs",
                    path.display()
                );
            }
        }
    }
}

/// The library's catalogue: the owner's, every component drawn with ui.css itself.
#[tokio::test]
async fn the_ui_catalogue_shows_the_library() {
    let s = TestServer::start().await;
    let cookie = s.owner_cookie().await;
    let page = owner_page(&s, &cookie, "/ui").await;
    assert!(page.contains("<h1>UI library</h1>"), "{page}");
    assert!(page.contains(r#"<button type="button" class="btn danger-solid">Delete</button>"#), "{page}");
    assert!(page.contains(r#"class="ibtn busy" disabled"#), "{page}");
    assert!(page.contains(r#"<symbol id="i-close" viewBox="0 0 24 24">"#), "the sprite: {page}");
    assert!(page.contains(r#"<input value="vinculum!" aria-invalid="true""#), "{page}");
    assert!(page.contains(r#"<span class="numf"><input id="ui-a" class="num" name="jobs" type="number" min="1" max="8" step="1" value="3">"#), "{page}");
    assert!(page.contains(r#"<span class="numf"><input id="ui-b" class="num" name="max" type="number" min="1" max="8" step="1" value="8">"#), "{page}");
    assert!(page.contains(r#"title="close">"#) && page.contains(r#"title="alert">"#), "every icon: {page}");
    // Everything the pages are built from since part 6: cards, navigation, the dialog, tables and
    // rows, messages and numbers, icon tiles (T-119).
    for id in ["ui-card", "ui-nav", "ui-table", "ui-msg"] {
        assert!(page.contains(&format!(r#"id="{id}""#)), "{id}: {page}");
    }
    assert!(
        page.contains(r#"<div class="dlg sm ui-dlg" role="presentation">"#)
            && page.contains(r#"<p class="notice bad">"#),
        "{page}"
    );
    // Works, the split button, a form from a button, a group that folds (M16.2).
    assert!(
        page.contains(r#"class="btn sm ok""#)
            && page.contains(r#"<div class="btn-split">"#)
            && page.contains(r#"class="dd-menu form""#)
            && page.contains(r#"<details class="fold" open>"#),
        "{page}"
    );
    // The owner's words, and a menu's group head (M16.1).
    assert!(
        page.contains(r#"<blockquote class="quote sm">"#) && page.contains(r#"<p class="dd-group label">"#),
        "{page}"
    );
    // Link chips and the pager (T-210).
    assert!(page.contains(r##"<a class="chip link" href="#ui-chip">"##), "{page}");
    assert!(
        page.contains(r#"<nav class="pager links" aria-label="Pages of the example"><span class="sub pager-range">1–20 of 214</span><span class="sub mono">page 1 of 11</span><span class="ibtn off" aria-hidden="true">"#)
            && page.contains(r##"<a class="ibtn" href="/ui?page=2#ui-table" title="Older jobs" aria-label="Older jobs">"##),
        "{page}"
    );
    let res = s.http.get(s.url("/ui")).send().await.unwrap();
    assert_ne!(res.status(), reqwest::StatusCode::OK, "not without signing in");
}

#[tokio::test]
async fn tasks_on_a_milestone_with_steps_show_on_plan() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    for (name, args) in [
        ("add_milestone", json!({"name": "M4", "title": "Keybinds"})),
        ("add_milestone", json!({"name": "K1", "title": "Read binds", "step_of": "M4"})),
        ("add_task", json!({"milestone": "K1", "title": "Parse binds", "estimate": "1 h"})),
        ("add_task", json!({"milestone": "M4", "title": "Document binds", "estimate": "1 h"})),
    ] {
        let (ok, r) = mcp_tool(&s, &token, &sess, name, args).await;
        assert!(ok, "{name}: {r}");
    }
    let cookie = s.owner_cookie().await;
    // The step's board, and the milestone's own task one entry further down the rail.
    let board = owner_page(&s, &cookie, "/p/vinculum/plan/M4").await;
    assert!(board.contains("Parse binds"), "{board}");
    assert!(board.contains(r#"href="/p/vinculum/plan/M4?step=none""#) && board.contains("Not in a step"), "{board}");
    let own = owner_page(&s, &cookie, "/p/vinculum/plan/M4?step=none").await;
    assert!(own.contains("Document binds"), "{own}");
    // Its page's path opens Plan there.
    let page = owner_page(&s, &cookie, "/p/vinculum/T-2").await;
    assert!(page.contains(r#"href="/p/vinculum/plan/M4?step=none""#), "{page}");
}

#[tokio::test]
async fn with_a_proxy_secret_a_local_process_cant_name_its_address() {
    let s = TestServer::start_with(|c, _| c.proxy_secret = "caddy-only".into()).await;
    let ask = |secret: Option<&'static str>| {
        let mut req = s.http.post(s.url("/api/approvals")).header("x-forwarded-for", "10.0.8.2");
        if let Some(secret) = secret {
            req = req.header("x-agentflow-proxy", secret);
        }
        req.json(&json!({"action": "connect", "machine": "viber@mothman"})).send()
    };
    let from = || {
        s.db.call(|c| {
            Ok(c.query_row("SELECT ip FROM approvals ORDER BY id DESC LIMIT 1", [], |r| r.get::<_, String>(0))?)
        })
    };
    // Any local process reaches the listener like the proxy does; without the secret its
    // X-Forwarded-For isn't believed.
    assert_eq!(ask(None).await.unwrap().status(), StatusCode::OK);
    assert_eq!(from().await.unwrap(), "127.0.0.1");
    assert_eq!(ask(Some("guess")).await.unwrap().status(), StatusCode::OK);
    assert_eq!(from().await.unwrap(), "127.0.0.1");
    assert_eq!(ask(Some("caddy-only")).await.unwrap().status(), StatusCode::OK);
    assert_eq!(from().await.unwrap(), "10.0.8.2");
}

#[tokio::test]
async fn anonymous_requests_dont_fill_the_log_or_the_database() {
    let s = TestServer::start().await;
    for _ in 0..60 {
        s.http.get(s.url("/login")).send().await.unwrap();
        s.http.post(s.url("/api/setup/redeem")).json(&json!({"code": "nope", "machine": "m"})).send().await.unwrap();
    }
    // 20 a minute for the requests and 20 for the refused codes (twice, if the minute turned).
    let rows: i64 = s.db.call(|c| Ok(c.query_row("SELECT count(*) FROM log", [], |r| r.get(0))?)).await.unwrap();
    assert!((1..=80).contains(&rows), "{rows} rows from one anonymous address in a minute");

    // A connect request carries no arguments, and nothing near the 2 MB a JSON body may have.
    let ask = |args: Value| {
        s.http.post(s.url("/api/approvals")).json(&json!({"action": "connect", "machine": "m", "args": args})).send()
    };
    assert_eq!(ask(json!({"p": "x"})).await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert_eq!(ask(json!({"p": "x".repeat(100_000)})).await.unwrap().status(), StatusCode::PAYLOAD_TOO_LARGE);
    let kept: i64 = s.db.call(|c| Ok(c.query_row("SELECT count(*) FROM approvals", [], |r| r.get(0))?)).await.unwrap();
    assert_eq!(kept, 0);
}

#[tokio::test]
async fn rotate_is_offered_only_where_the_token_file_is() {
    let s = TestServer::start().await;
    let cookie = s.owner_cookie().await;
    s.token(TokenKind::Project, Some("herelocal")).await;
    s.token(TokenKind::Project, Some("faraway")).await;
    let builder =
        s.db.call(|c| {
            c.execute("UPDATE projects SET machine = 'not-this-host' WHERE slug = 'faraway'", [])?;
            Ok(auth::create_token(c, TokenKind::Builder, "bigbox", None)?.0)
        })
        .await
        .unwrap();
    let page =
        s.http.get(s.url("/settings?s=agents")).header(COOKIE, &cookie).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("herelocal") && page.contains("faraway") && page.contains("bigbox"), "{page}");
    assert_eq!(page.matches("/rotate\"").count(), 1, "only the local project's token has Rotate");
    // And the endpoint refuses the others.
    let res = s
        .http
        .post(s.url(&format!("/settings/tokens/{builder}/rotate")))
        .header(COOKIE, &cookie)
        .header("origin", &s.base)
        .send()
        .await
        .unwrap();
    assert!(!res.status().is_redirection() && !res.status().is_success(), "{}", res.status());
}

/// Withdrawing a question gives back the tasks it blocked, as answering or archiving it does:
/// they stayed blocked by nothing, and the step couldn't be concluded.
#[tokio::test]
async fn withdrawing_a_question_unblocks_its_tasks() {
    let s = TestServer::start().await;
    let token = s.token(TokenKind::Project, Some("vinculum")).await;
    let sess = mcp_session(&s, &token).await;
    let call = |name: &'static str, args: Value| {
        let (s, token, sess) = (&s, token.clone(), sess.clone());
        async move { mcp_tool(s, &token, &sess, name, args).await }
    };
    assert!(call("add_milestone", json!({"name": "M1", "title": "Keybinds"})).await.0);
    assert!(call("add_milestone", json!({"name": "K1", "title": "Read binds", "step_of": "M1"})).await.0);
    let (ok, r) = call("add_tasks", json!({"milestone": "K1", "tasks": [{"title": "Parse", "estimate": "1 h"}]})).await;
    assert!(ok, "{r}");
    let (ok, q) = call("ask_question", json!({"title": "Wide or narrow?", "blocks": ["T-1"]})).await;
    assert!(ok, "{q}");
    let (ok, w) = call("withdraw_question", json!({"id": "Q-1", "reason": "found it in the docs"})).await;
    assert!(ok, "{w}");
    let (_, t1) = call("get_record", json!({"id": "T-1"})).await;
    assert_eq!(t1["state"], "todo", "{t1}");
    assert!(call("update_task", json!({"id": "T-1", "state": "done"})).await.0);
    let (ok, c) = call("conclude_step", json!({"step": "K1", "summary": "Binds are read."})).await;
    assert!(ok, "{c}");
}
