//! What the owner needs to test: each project's agent keeps a log of it, with categories of its
//! own choosing (the owner, 2026-11-11: "all agents held a log of things that i need to test with
//! a category (that the agent can define). all open log items will be attached to a new release. i
//! can talk to the agent to resolve these", "this should be in every release and old ones that
//! havent been checked yet are also put into newer builds").
//!
//! An item is open until the owner tested it (ticked and sent on a page, or told the agent, who
//! marks it), or the agent drops it. Every new release links the items open at the time, and keeps
//! the first release an item came with; release and build pages show what's still open.

use rusqlite::Connection;
use serde_json::{Map, Value, json};

use crate::records::{self, Kind, NewRecord, Patch, Query, Record};
use crate::util::now_ms;

pub fn check_category(c: &str) -> anyhow::Result<String> {
    let c = c.split_whitespace().collect::<Vec<_>>().join(" ");
    if c.is_empty() || c.chars().count() > 40 {
        anyhow::bail!("give it a category of a few words (up to 40 characters), e.g. Import");
    }
    Ok(c)
}

/// Adds something to test.
pub fn add(
    conn: &Connection,
    project_id: i64,
    author: &str,
    title: &str,
    how: &str,
    category: &str,
) -> anyhow::Result<Record> {
    let category = check_category(category)?;
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 200 {
        anyhow::bail!("say what to test in a line (up to 200 characters)");
    }
    let mut data = Map::new();
    data.insert("category".into(), category.into());
    records::create(
        conn,
        project_id,
        author,
        NewRecord {
            kind: Some(Kind::Test),
            title: title.to_owned(),
            body: how.trim().to_owned(),
            state: Some("open".into()),
            data,
            ..Default::default()
        },
    )
}

/// A project's open items, by category (in the order categories first appeared), oldest first.
pub fn open_items(conn: &Connection, project_id: i64) -> anyhow::Result<Vec<Record>> {
    let mut items = records::query(
        conn,
        &Query {
            project_id: Some(project_id),
            kinds: vec![Kind::Test],
            states: vec!["open".into()],
            limit: 0,
            ..Default::default()
        },
    )?;
    items.sort_by_key(|r| r.created_at);
    let mut order: Vec<String> = Vec::new();
    for r in &items {
        let c = r.str("category").to_owned();
        if !order.contains(&c) {
            order.push(c);
        }
    }
    items.sort_by_key(|r| (order.iter().position(|c| c == r.str("category")).unwrap_or(0), r.created_at));
    Ok(items)
}

/// How many are open, per project.
pub fn open_count(conn: &Connection, project_id: i64) -> anyhow::Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM records WHERE project_id = ?1 AND kind = 'test' AND state = 'open'",
        [project_id],
        |r| r.get(0),
    )?)
}

/// The ones tested or dropped, newest first.
pub fn done_items(conn: &Connection, project_id: i64, limit: i64) -> anyhow::Result<Vec<Record>> {
    let mut items = records::query(
        conn,
        &Query {
            project_id: Some(project_id),
            kinds: vec![Kind::Test],
            states: vec!["tested".into(), "dropped".into()],
            limit: 0,
            ..Default::default()
        },
    )?;
    items.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    items.truncate(limit as usize);
    Ok(items)
}

/// A new release takes every open item along: linked to it, and the first release an item came
/// with is kept on the item.
pub fn attach_to_release(conn: &Connection, release: &Record, author: &str) -> anyhow::Result<usize> {
    let tag = release.str("tag").to_owned();
    let label = if tag.is_empty() { release.title.clone() } else { tag };
    let items = open_items(conn, release.project_id)?;
    for item in &items {
        records::link(conn, item, release, "tests", author)?;
        if item.str("first_release").is_empty() {
            let mut data = Map::new();
            data.insert("first_release".into(), label.clone().into());
            records::update(conn, item, author, Patch { data, quiet: true, ..Default::default() })?;
        }
    }
    Ok(items.len())
}

/// The owner ticked these and sent: tested, it works. The agent hears it, with the owner's note.
pub fn owner_tested(conn: &Connection, project_id: i64, ids: &[i64], note: &str) -> anyhow::Result<Vec<Record>> {
    let mut done = Vec::new();
    for &id in ids {
        let r = records::get(conn, id)?;
        if r.project_id != project_id || r.kind != Kind::Test || r.state != "open" {
            continue;
        }
        let mut data = Map::new();
        data.insert("result".into(), "works".into());
        data.insert("tested_at".into(), now_ms().into());
        data.insert("tested_by".into(), "owner".into());
        if !note.trim().is_empty() {
            data.insert("owner_note".into(), note.trim().into());
        }
        let (r, _) = records::update(
            conn,
            &r,
            "owner",
            Patch {
                state: Some("tested".into()),
                data,
                quiet: true,
                note: "tested by the owner: works".into(),
                ..Default::default()
            },
        )?;
        done.push(r);
    }
    if !done.is_empty() {
        let list = done.iter().map(|r| format!("{} {}", r.short_id(), r.title)).collect::<Vec<_>>().join("; ");
        let what = if note.trim().is_empty() {
            format!("Owner tested, works: {list}")
        } else {
            format!("Owner tested, works: {list}. Note: \"{}\"", note.trim())
        };
        records::event(conn, Some(project_id), done.first().map(|r| r.id), "test.tested", &what, "owner")?;
    }
    Ok(done)
}

/// The agent, after talking to the owner: tested and works, a problem (the item stays open, with
/// the problem noted), or dropped (no longer needed).
pub fn agent_resolve(
    conn: &Connection,
    item: &Record,
    result: &str,
    note: &str,
    author: &str,
) -> anyhow::Result<Record> {
    if item.kind != Kind::Test {
        anyhow::bail!("{} isn't something to test", item.ref_id);
    }
    if item.state != "open" {
        anyhow::bail!("{} is {} already", item.ref_id, item.state);
    }
    let note = note.trim();
    let mut data = Map::new();
    let state = match result {
        "works" => {
            data.insert("result".into(), "works".into());
            data.insert("tested_at".into(), now_ms().into());
            data.insert("tested_by".into(), "owner, told the agent".into());
            if !note.is_empty() {
                data.insert("owner_note".into(), note.into());
            }
            Some("tested".to_owned())
        }
        "problem" => {
            if note.is_empty() {
                anyhow::bail!("say what the problem is, as the owner put it");
            }
            let mut problems = item.data.get("problems").and_then(Value::as_array).cloned().unwrap_or_default();
            problems.push(json!({"at": now_ms(), "note": note}));
            data.insert("problems".into(), Value::Array(problems));
            None
        }
        "dropped" => {
            if !note.is_empty() {
                data.insert("dropped_because".into(), note.into());
            }
            Some("dropped".to_owned())
        }
        _ => anyhow::bail!("the result is works, problem or dropped"),
    };
    let (r, _) = records::update(conn, item, author, Patch { state, data, ..Default::default() })?;
    Ok(r)
}
