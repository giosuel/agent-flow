//! `agentflow admin demo`: a complete made-up project, "Lumen (demo)", to try every page with
//! before real agents use AgentFlow (the owner, 2026-11-11: "please create a complete fake project so
//! I can test the UI before the onboarding"). Archive it from its page when done.
//!
//! Everything a real agent would leave: details, milestones with steps, tasks in every state,
//! questions (open, blocking, answered), decisions and an ADR, a stack of PRs with Merge buttons
//! and a release with a tag button (all of them only `echo`), two builds you can run (one with
//! feedback, one waiting for it), screenshots, a conversation with the consultant, and a week of
//! token usage. It writes straight to the database, as its own author, with no token, so nothing
//! is sent to GitHub and no notification goes out.

use std::path::Path;

use anyhow::bail;
use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};

use crate::github::PrInfo;
use crate::projects::{self, MilestonePatch, NewMilestone, ProjectPatch};
use crate::records::{self, Kind, NewRecord, Record};
use crate::util::now_ms;

pub const SLUG: &str = "lumen-demo";
const NAME: &str = "Lumen (demo)";
const REPO: &str = "giosuel/lumen-demo";
const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

// A local shorthand for records::create; a struct for its arguments would only add noise.
#[allow(clippy::too_many_arguments)]
fn rec(
    c: &Connection,
    project: i64,
    kind: Kind,
    title: &str,
    body: &str,
    state: &str,
    milestone: Option<i64>,
    data: Value,
) -> anyhow::Result<Record> {
    let data: Map<String, Value> = match data {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    records::create(
        c,
        project,
        SLUG,
        NewRecord {
            kind: Some(kind),
            title: title.into(),
            body: body.into(),
            state: Some(state.into()),
            milestone_id: milestone,
            data,
            ..Default::default()
        },
    )
}

/// Creates the demo project; refuses if it's there already.
pub fn create(c: &mut Connection, data_dir: &Path) -> anyhow::Result<String> {
    if projects::get(c, SLUG)?.is_some() {
        bail!("{SLUG} exists already; archive it on its page, or look at it at /p/{SLUG}");
    }
    let p = projects::create(c, SLUG, NAME, "owner")?;
    projects::update(
        c,
        p.id,
        &ProjectPatch {
            description: Some("A made-up photo organiser for Linux, to try AgentFlow with. Nothing here is real: the buttons only echo, the GitHub links go nowhere.".into()),
            checkout: Some("~/lumen-demo".into()),
            machine: Some("giosuel.ch".into()),
            stack: Some(vec!["Rust".into(), "GTK 4".into(), "SQLite".into(), "libraw".into()]),
            platform: Some("Linux · GNOME and Hyprland".into()),
            model: Some("Claude Opus 5.5".into()),
            effort: Some("high".into()),
            pm_rules: Some(vec!["Every PR in a stack is based on the PR before it.".into(), "Mockups before any UI.".into()]),
            git_rules: Some("Conventional Commits".into()),
            ports: Some(vec!["8990 thumbnail server".into()]),
            build_product: Some(SLUG.into()),
            ..Default::default()
        },
        SLUG,
    )?;
    projects::hello(c, p.id, "Lumen", "giosuel.ch", "Claude Opus 5.5", "high")?;
    projects::set_agent_note(c, p.id, "T-4 and T-5, with 2 subagents")?;

    // Milestones: three done with their tags, M3 under way with steps, two ahead.
    let ms = |c: &Connection, name: &str, title: &str, parent: Option<i64>| {
        projects::add_milestone(c, p.id, &NewMilestone { name, title, description: "", parent, before: None }, SLUG)
    };
    let set = |c: &Connection, m: &projects::Milestone, state: &str, tag: Option<&str>| {
        let patch = MilestonePatch { state: Some(state.into()), tag: tag.map(Into::into), ..Default::default() };
        projects::update_milestone(c, m, &patch, SLUG)
    };
    for (n, t, tag) in
        [("M0", "Skeleton", "v0.0.1-m0"), ("M1", "Library import", "v0.0.2-m1"), ("M2", "Thumbnails", "v0.0.3-m2")]
    {
        let m = ms(c, n, t, None)?;
        set(c, &m, "closed", Some(tag))?;
    }
    let m3 = ms(c, "M3", "Albums and tags", None)?;
    set(c, &m3, "active", None)?;
    let s1 = ms(c, "S1", "Album model", Some(m3.id))?;
    set(c, &s1, "closed", None)?;
    c.execute(
        "UPDATE milestones SET summary = ?1 WHERE id = ?2",
        rusqlite::params![
            "Albums table, migrations and smart albums are in. The migration took less than planned: the schema from M1 already had a join table.",
            s1.id
        ],
    )?;
    let s2 = ms(c, "S2", "Album sidebar", Some(m3.id))?;
    set(c, &s2, "active", None)?;
    let s3 = ms(c, "S3", "Tag editor", Some(m3.id))?;
    let s4 = ms(c, "S4", "End-of-M3 review", Some(m3.id))?;
    projects::set_review(
        c,
        &s4,
        &json!({"reviewers": ["correctness", "UX", "performance"], "commit": "9f3c2e1",
                "owner_words": "rate the issues by importance and implement the ones you think are necessary", "reports": []}),
    )?;
    let m4 = ms(c, "M4", "Search", None)?;
    ms(c, "M5", "Packaging", None)?;

    // Tasks in every state.
    let body =
        "Acceptance: works with the keyboard alone, and with 50,000 photos.\n\n- `src/albums/`\n- checked under Xvfb";
    let mut tasks = Vec::new();
    for (m, title, state) in [
        (s1.id, "Albums table and migrations", "done"),
        (s1.id, "Smart albums from a saved search", "done"),
        (s2.id, "Album list in the sidebar", "review"),
        (s2.id, "Drag photos onto an album", "doing"),
        (s2.id, "Rename an album in place", "doing"),
        (s2.id, "Sidebar layout", "blocked"),
        (s3.id, "Tag editor dialog", "todo"),
        (s3.id, "Suggest tags from EXIF data", "todo"),
        (m4.id, "Face grouping", "parked"),
    ] {
        tasks.push(rec(c, p.id, Kind::Task, title, body, state, Some(m), json!({"estimate": "2 h"}))?);
    }
    // The review's findings: ranked, one skipped with its reason.
    for (title, importance, state, extra) in [
        ("Import can follow a symlink out of the library folder", "high", "todo", json!({})),
        ("Sidebar re-renders every album on each thumbnail load", "medium", "todo", json!({})),
        ("Album colours aren't in the theme file", "low", "skipped", json!({"skip_reason": "the palette is reworked in M5 anyway"})),
    ] {
        let mut data = json!({"estimate": "1 h", "source": "review", "importance": importance});
        if let (Some(d), Some(e)) = (data.as_object_mut(), extra.as_object()) {
            d.extend(e.clone());
        }
        rec(c, p.id, Kind::Task, title, "", state, Some(s4.id), data)?;
    }

    // Questions: one blocking a task, with options; a plain one; one answered.
    let q1 = rec(
        c,
        p.id,
        Kind::Question,
        "Album sidebar: a list or a grid of covers?",
        "Both are on the mockups (screenshots S-1 and S-2). **A** is a plain list of names, like Files; **B** shows each album's cover in a two-column grid.",
        "open",
        Some(s2.id),
        json!({"options": [
            {"title": "A: a list of names", "detail": "Fits 40 albums without scrolling; quick to scan."},
            {"title": "B: a grid of covers", "detail": "Prettier, but shows only about 10 at once."}
        ], "recommended": 1}),
    )?;
    records::link(c, &q1, &tasks[5], "blocks", SLUG)?;
    rec(
        c,
        p.id,
        Kind::Question,
        "May the thumbnail server use port 8990?",
        "It only listens on 127.0.0.1, for the importer's previews.",
        "open",
        None,
        json!({}),
    )?;
    let q3 = rec(
        c,
        p.id,
        Kind::Question,
        "Which base should the stack of album PRs use?",
        "",
        "open",
        None,
        json!({"options": [{"title": "Each PR on the one before", "detail": ""}, {"title": "All on main", "detail": ""}], "recommended": 1}),
    )?;
    let q3 = records::answer_question(c, &q3, Some(1), "each on the one before, like always", "owner")?;

    // Decisions and an ADR.
    let said = |days: i64| now_ms() - days * DAY;
    let d1 = rec(
        c,
        p.id,
        Kind::Decision,
        "Thumbnails are cached on disk, not in the database",
        "SQLite grew to 3 GB with them inside.",
        "current",
        None,
        json!({"owner_words": "keep the db small, put thumbnails in ~/.cache", "said_at": said(9), "source": "owner"}),
    )?;
    let d2 = rec(
        c,
        p.id,
        Kind::Decision,
        "Stacked PRs are based on the PR before them",
        "",
        "current",
        None,
        json!({"owner_words": "each on the one before, like always", "said_at": said(1), "source": "owner"}),
    )?;
    records::link(c, &d2, &q3, "answers", SLUG)?;
    rec(
        c,
        p.id,
        Kind::Decision,
        "Albums get their own milestone (M3)",
        "",
        "current",
        None,
        json!({"owner_words": "do albums before search", "said_at": said(5), "source": "owner"}),
    )?;
    let adr = rec(
        c,
        p.id,
        Kind::Adr,
        "ADR 0007: one SQLite file per library",
        "",
        "current",
        None,
        json!({"repo": REPO, "path": "docs/adr/0007-one-db-per-library.md", "adr_number": 7}),
    )?;
    records::link(c, &adr, &d1, "records", SLUG)?;

    // A stack of PRs (#12 ← #13 ← #14), a draft, and a merged one; buttons only echo.
    let mut prs = Vec::new();
    for (n, title, base, draft, state) in [
        (11, "feat: albums table and migrations", "main", false, "merged"),
        (12, "feat: album list in the sidebar", "main", false, "open"),
        (13, "feat: drag photos onto an album", "feat/album-list", false, "open"),
        (14, "feat: rename an album in place", "feat/album-drag", false, "open"),
        (15, "wip: tag editor dialog", "main", true, "open"),
    ] {
        let head = match n {
            12 => "feat/album-list",
            13 => "feat/album-drag",
            14 => "feat/album-rename",
            _ => "feat/other",
        };
        let info = PrInfo {
            number: n,
            title: title.into(),
            url: format!("https://github.com/{REPO}/pull/{n}"),
            state: state.into(),
            draft,
            base: base.into(),
            head: head.into(),
            mergeable: "mergeable".into(),
            ci: "passing".into(),
        };
        let (r, _) = crate::prs::sync(c, &projects::get_by_id(c, p.id)?, REPO, &info, SLUG)?;
        let r = if state == "open" && !draft { crate::prs::set_ready(c, &r, true, "", SLUG)? } else { r };
        if state == "open" && !draft {
            let command = format!("echo 'Demo: nothing is merged (PR #{n} of {REPO}).'");
            let b = crate::actions::NewButton {
                label: "Merge",
                kind: crate::actions::ButtonKind::Merge,
                command: &command,
                workdir: "~",
                requires: &[],
            };
            crate::actions::add(c, &r, &b, SLUG)?;
        }
        prs.push(r);
    }
    records::link(c, &prs[1], &tasks[2], "implements", SLUG)?;
    records::link(c, &prs[2], &tasks[3], "implements", SLUG)?;
    records::link(c, &prs[3], &tasks[4], "implements", SLUG)?;
    let release = rec(
        c,
        p.id,
        Kind::Release,
        "M3: Albums and tags",
        "",
        "waiting",
        Some(m3.id),
        json!({"tag": "v0.0.4-m3", "waits_for": [prs[1].ref_id, prs[2].ref_id, prs[3].ref_id]}),
    )?;
    let requires = vec![prs[3].ref_id.clone()];
    let b = crate::actions::NewButton {
        label: "Push tag v0.0.4-m3",
        kind: crate::actions::ButtonKind::Tag,
        command: "echo 'Demo: no tag is pushed (v0.0.4-m3).'",
        workdir: "~",
        requires: &requires,
    };
    crate::actions::add(c, &release, &b, SLUG)?;

    // Two builds that `flow-run lumen-demo` really runs: B-1 tried, B-2 waiting for feedback.
    let b1 =
        build(c, p.id, data_dir, "M2 thumbnails", "f3a91c2", &["Thumbnails: open a folder of 500 photos"], "tried")?;
    for (title, state, words) in [
        ("Thumbnails", "works", "fast, even on the big folder"),
        ("General note", "note", "the window could remember its size"),
    ] {
        let mut data = Map::new();
        data.insert("owner_words".into(), words.into());
        data.insert("build".into(), b1.ref_id.clone().into());
        let new = NewRecord {
            kind: Some(Kind::Feedback),
            title: title.into(),
            state: Some(state.into()),
            data,
            ..Default::default()
        };
        let fb = records::create(c, p.id, "owner", new)?;
        records::link(c, &fb, &b1, "feedback_on", "owner")?;
    }
    build(
        c,
        p.id,
        data_dir,
        "Album sidebar",
        "8b04d7e",
        &["Albums: make an album and drag three photos onto it", "Rename: double-click an album's name"],
        "published",
    )?;

    // Screenshots, drawn here: two mockups, a test build, a spike test and a bug.
    for (i, (name, category, desc, link)) in [
        ("Album sidebar, variant A: a list", "mockup", "For Q-1. Names only, 40 fit on a laptop screen.", None),
        ("Album sidebar, variant B: covers", "mockup", "For Q-1. Two columns of covers.", None),
        ("Sidebar in the B-2 build", "test build", "", Some(2usize)),
        ("Thumbnail grid at 50,000 photos", "spike test", "Scrolling stays at 60 fps with the disk cache.", None),
        ("Rename field cut off in narrow windows", "bug", "Below 700 px the field overflows the sidebar.", Some(4)),
    ]
    .into_iter()
    .enumerate()
    {
        let png = mock_png(i);
        let data = json!({"category": category, "mime": "image/png", "ext": "png", "size": png.len(), "width": 640, "height": 400});
        let r = rec(c, p.id, Kind::Screenshot, name, desc, "current", None, data)?;
        let path = crate::screenshots::path(data_dir, SLUG, r.number, "png");
        std::fs::create_dir_all(path.parent().unwrap_or(data_dir))?;
        std::fs::write(&path, &png)?;
        if let Some(t) = link {
            records::link(c, &r, &tasks[t], "shows", SLUG)?;
        }
    }

    // A week of token usage, so the agent card and Usage have something to show.
    let now = now_ms();
    for i in 0..300i64 {
        let at = now - (i * 7 * DAY / 300) - (i % 7) * 60_000;
        c.execute(
            "INSERT OR IGNORE INTO usage (message_id, at, folder, cwd, session, subagent, model, project_id, task_id,
               input, output, cache_write, cache_read)
             VALUES (?1, ?2, '-home-viber-lumen-demo', '', 'lumen-demo-session', ?3, 'claude-opus-5-5', ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                format!("demo-{i}"),
                at,
                i % 4 == 0,
                p.id,
                tasks[(i as usize) % tasks.len()].id,
                40 + i % 90,
                600 + (i * 37) % 2400,
                (i * 911) % 12_000,
                40_000 + (i * 1_733) % 90_000
            ],
        )?;
    }
    Ok(format!("Created {NAME} at /p/{SLUG}: archive it from its page when you're done."))
}

/// A real build: a folder with a script that says it's the demo, packed like `agentflow publish`.
fn build(
    c: &Connection,
    project: i64,
    data_dir: &Path,
    summary: &str,
    commit: &str,
    features: &[&str],
    state: &str,
) -> anyhow::Result<Record> {
    let src = tempfile::tempdir()?;
    let script = src.path().join(SLUG);
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho 'Lumen demo build: {summary}. Nothing to see; it only shows that flow-run works.'\n"),
    )?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    }
    let dir = crate::builds::dir(data_dir);
    std::fs::create_dir_all(dir.join(SLUG))?;
    let name = crate::builds::build_name(time::OffsetDateTime::now_utc(), "main", commit);
    let archive = crate::builds::archive_path(&dir, SLUG, &name);
    crate::builds::pack(src.path(), std::fs::File::create(&archive)?)?;
    let manifest = crate::builds::inspect(std::fs::File::open(&archive)?)?;
    std::fs::write(crate::builds::manifest_path(&dir, SLUG, &name), serde_json::to_vec(&manifest)?)?;
    let feats: Vec<Value> = features.iter().filter_map(|f| crate::builds::feature(f)).collect();
    rec(
        c,
        project,
        Kind::Build,
        summary,
        "",
        state,
        None,
        json!({"product": SLUG, "name": name, "commit": commit, "branch": "main", "entry": SLUG,
               "size": manifest.total, "files_count": manifest.files.len(), "features": feats,
               "ready_for_feedback": state == "published"}),
    )
}

// ---------------------------------------------------------------------------------------------
// A small PNG writer, for the demo's screenshots: flat wireframes of an app window.

fn crc32(bytes: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in bytes {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in bytes {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// RGB pixels to PNG, with stored (uncompressed) deflate blocks: simple and always valid.
pub fn png(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width as usize * 3 + 1) * height as usize);
    for row in rgb.chunks(width as usize * 3) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, block) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        z.extend_from_slice(&(block.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        z.extend_from_slice(block);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// A 640×400 wireframe: a title bar, a sidebar, a grid of photos, in one of five looks.
fn mock_png(variant: usize) -> Vec<u8> {
    let (w, h) = (640u32, 400u32);
    let mut px = vec![0u8; (w * h * 3) as usize];
    let mut fill = |x0: u32, y0: u32, x1: u32, y1: u32, rgb: [u8; 3]| {
        for y in y0.min(h)..y1.min(h) {
            for x in x0.min(w)..x1.min(w) {
                let i = ((y * w + x) * 3) as usize;
                px[i..i + 3].copy_from_slice(&rgb);
            }
        }
    };
    let accents = [[167, 139, 250], [96, 165, 250], [74, 222, 128], [251, 191, 36], [248, 113, 113]];
    let accent = accents[variant % accents.len()];
    fill(0, 0, w, h, [18, 18, 24]);
    fill(0, 0, w, 36, [10, 10, 12]);
    for (i, x) in [14u32, 34, 54].into_iter().enumerate() {
        fill(x, 13, x + 10, 23, [[248, 113, 113], [251, 191, 36], [74, 222, 128]][i]);
    }
    let side = if variant == 1 { 220 } else { 170 };
    fill(0, 36, side, h, [15, 15, 19]);
    for i in 0..9u32 {
        let y = 52 + i * 34;
        if variant == 1 {
            // Covers: two columns of squares.
            for col in 0..2u32 {
                fill(14 + col * 100, y, 104 + col * 100, y + 28, if i == 1 { accent } else { [44, 44, 54] });
            }
        } else {
            fill(14, y, side - 14, y + 22, if i == 1 { accent } else { [38, 38, 46] });
        }
    }
    if variant == 4 {
        // The bug: a field running past the sidebar's edge.
        fill(14, 120, side + 60, 146, [248, 113, 113]);
    }
    let (cols, gap) = if variant == 3 { (8u32, 6u32) } else { (5, 10) };
    let cell = (w - side - gap * (cols + 1)) / cols;
    for r in 0..6u32 {
        for col in 0..cols {
            let x = side + gap + col * (cell + gap);
            let y = 52 + r * (cell * 3 / 4 + gap);
            let shade = 50 + ((r * 7 + col * 13 + variant as u32 * 5) % 40) as u8;
            fill(x, y, x + cell, y + cell * 3 / 4, [shade, shade, shade + 14]);
        }
    }
    png(w, h, &px)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_a_whole_project_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("flow.db")).unwrap();
        let d = dir.path().to_owned();
        db.call_blocking(move |c| {
            create(c, &d)?;
            assert!(create(c, &d).is_err(), "only once");
            let all = records::query(c, &records::Query { limit: 0, ..Default::default() })?;
            for kind in [
                Kind::Task,
                Kind::Question,
                Kind::Decision,
                Kind::Adr,
                Kind::Pr,
                Kind::Release,
                Kind::Build,
                Kind::Feedback,
                Kind::Screenshot,
            ] {
                assert!(all.iter().any(|r| r.kind == kind), "a {kind}");
            }
            let png = std::fs::read(crate::screenshots::path(&d, SLUG, 1, "png"))?;
            assert_eq!(crate::screenshots::sniff(&png), Some(("image/png", "png")));
            assert_eq!(crate::screenshots::png_size(&png), Some((640, 400)));
            let waiting = crate::waiting::load(c)?;
            assert_eq!(waiting.merges.len(), 1, "the stack's first PR can be merged now");
            assert_eq!(waiting.builds.len(), 1);
            Ok(())
        })
        .unwrap();
    }
}
