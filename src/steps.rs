//! How work moves through steps (the owner, 2026-11-12): a step starts by itself when its first
//! task goes to doing; the agent concludes it, with a few lines, once its tasks are done. Open PRs
//! and untested builds never hold a step: the agent stacks them and moves on. A task is blocked
//! either by a question (anything that needs the owner: "if its facing me, it always links to a
//! question") or by a short note (an agent's review, a remote build); answering the question
//! unblocks it.

use anyhow::bail;
use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};

use crate::projects::{self, Milestone};
use crate::records::{self, Kind, Patch, Query, Record, event};
use crate::util::now_ms;

/// Task states that keep a step from concluding: still to do, in progress, or waiting on
/// something. "review" (its PR is open) doesn't: open PRs never hold a step.
pub const OPEN: &[&str] = &["todo", "doing", "blocked"];

/// "2 h", "40 min", "1 h 30 min", "1.5h", "90m" → minutes. Estimates are the agent's own words,
/// but they have to be readable as a duration so actual times can be set against them.
pub fn parse_minutes(s: &str) -> Option<i64> {
    let s = s.trim().to_lowercase().replace(',', ".");
    if s.is_empty() {
        return None;
    }
    let mut total = 0.0;
    let mut num = String::new();
    let mut unit = String::new();
    let mut found = false;
    let mut flush = |num: &mut String, unit: &mut String| -> Option<()> {
        if num.is_empty() {
            return if unit.is_empty() { Some(()) } else { None };
        }
        let n: f64 = num.parse().ok()?;
        let factor = match unit.as_str() {
            "h" | "hr" | "hrs" | "hour" | "hours" => 60.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 1.0,
            "d" | "day" | "days" => 8.0 * 60.0,
            _ => return None,
        };
        total += n * factor;
        found = true;
        num.clear();
        unit.clear();
        Some(())
    };
    for ch in s.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            if !unit.is_empty() {
                flush(&mut num, &mut unit)?;
            }
            num.push(ch);
        } else if ch.is_alphabetic() {
            unit.push(ch);
        } else if ch.is_whitespace() {
            continue;
        } else {
            return None;
        }
    }
    flush(&mut num, &mut unit)?;
    (found && total > 0.0 && total < 60.0 * 24.0 * 30.0).then(|| total.round() as i64)
}

/// Where a task came from: planned with its step, or added while the step was under way.
pub const SOURCES: &[&str] = &["planned", "owner_feedback", "found", "review", "bug"];

/// A task went to doing: its step (and the step's milestone) start if they hadn't.
pub fn task_started(conn: &Connection, task: &Record, actor: &str) -> anyhow::Result<Vec<String>> {
    let mut started = Vec::new();
    let Some(mid) = task.milestone_id else { return Ok(started) };
    let m = projects::get_milestone(conn, mid)?;
    let mut chain = vec![m.clone()];
    if let Some(parent) = m.parent_id {
        chain.push(projects::get_milestone(conn, parent)?);
    }
    for m in chain {
        if m.state == "planned" {
            let now = now_ms();
            conn.execute(
                "UPDATE milestones SET state = 'active', started_at = coalesce(started_at, ?1), updated_at = ?1 WHERE id = ?2",
                params![now, m.id],
            )?;
            let what = if m.parent_id.is_some() { "Step" } else { "Milestone" };
            event(
                conn,
                Some(m.project_id),
                Some(task.id),
                "milestone.started",
                &format!("{what} {} started: {} went to doing", m.name, task.short_id()),
                actor,
            )?;
            started.push(m.name);
        }
    }
    Ok(started)
}

/// The tasks of one step (or of a milestone itself, without its steps), in the agent's order.
pub fn tasks_of(conn: &Connection, milestone_id: i64) -> anyhow::Result<Vec<Record>> {
    let m = projects::get_milestone(conn, milestone_id)?;
    let mut all = records::query(conn, &Query { project_id: Some(m.project_id), kinds: vec![Kind::Task], ..Default::default() })?;
    all.retain(|t| t.milestone_id == Some(milestone_id));
    sort_by_position(&mut all);
    Ok(all)
}

pub fn sort_by_position(tasks: &mut [Record]) {
    tasks.sort_by(|a, b| {
        a.position.unwrap_or(f64::MAX).partial_cmp(&b.position.unwrap_or(f64::MAX)).unwrap_or(std::cmp::Ordering::Equal)
    });
}

#[derive(Debug)]
pub struct Concluded {
    pub step: Milestone,
    pub moved: Vec<String>,
}

/// Concludes a step with the agent's summary. Its open tasks must be dealt with first (moved,
/// parked or dropped), or moved together with `move_open_to`.
pub fn conclude(
    conn: &Connection,
    step: &Milestone,
    summary: &str,
    move_open_to: Option<&Milestone>,
    actor: &str,
) -> anyhow::Result<Concluded> {
    if step.state == "closed" {
        bail!("{} is concluded already", step.name);
    }
    let summary = summary.trim();
    if summary.is_empty() {
        bail!("say in a few lines what the step brought (what shipped, what turned up, what's next)");
    }
    let open: Vec<Record> = tasks_of(conn, step.id)?.into_iter().filter(|t| OPEN.contains(&t.state.as_str())).collect();
    let mut moved = Vec::new();
    if !open.is_empty() {
        let Some(to) = move_open_to else {
            let list = open.iter().map(|t| format!("{} ({})", t.short_id(), t.state)).collect::<Vec<_>>().join(", ");
            bail!(
                "{} still has open tasks: {list}. Move each to another step, park it or drop it (update_task), \
                 or pass move_open_to to move them all",
                step.name
            );
        };
        if to.id == step.id || to.project_id != step.project_id {
            bail!("move_open_to is another step or milestone of the same project");
        }
        if to.state == "closed" {
            bail!("{} is concluded; move the open tasks to a step that isn't", to.name);
        }
        for t in &open {
            records::update(
                conn,
                t,
                actor,
                Patch {
                    milestone_id: Some(Some(to.id)),
                    note: format!("moved from {} when it concluded", step.name),
                    ..Default::default()
                },
            )?;
            moved.push(t.short_id());
        }
    }
    let now = now_ms();
    conn.execute(
        "UPDATE milestones SET state = 'closed', closed_at = ?1, summary = ?2, started_at = coalesce(started_at, ?1), updated_at = ?1
         WHERE id = ?3",
        params![now, crate::activity::redact(summary), step.id],
    )?;
    let first = summary.lines().next().unwrap_or("");
    let what = if step.parent_id.is_some() { "Step" } else { "Milestone" };
    let moved_note = if moved.is_empty() { String::new() } else { format!(" ({} moved on)", moved.join(", ")) };
    event(conn, Some(step.project_id), None, "milestone.concluded", &format!("{what} {} concluded: {first}{moved_note}", step.name), actor)?;
    Ok(Concluded { step: projects::get_milestone(conn, step.id)?, moved })
}

/// Deletes a task for good (the owner, 2026-11-12: "let agents freely edit steps and tasks, edit
/// or delete them"): its history and links go too; the event log keeps a line saying what it was.
pub fn delete_task(conn: &Connection, task: &Record, reason: &str, actor: &str) -> anyhow::Result<()> {
    if task.kind != Kind::Task {
        bail!("{} isn't a task; only tasks are deleted", task.ref_id);
    }
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("say why it's deleted, in reason");
    }
    conn.execute("DELETE FROM actions WHERE record_id = ?1", [task.id])?;
    conn.execute("DELETE FROM links WHERE from_id = ?1 OR to_id = ?1", [task.id])?;
    conn.execute("DELETE FROM revisions WHERE record_id = ?1", [task.id])?;
    conn.execute("UPDATE events SET record_id = NULL WHERE record_id = ?1", [task.id])?;
    conn.execute("UPDATE timings SET record_id = NULL WHERE record_id = ?1", [task.id])?;
    conn.execute("UPDATE usage SET task_id = NULL WHERE task_id = ?1", [task.id])?;
    conn.execute("DELETE FROM records WHERE id = ?1", [task.id])?;
    event(
        conn,
        Some(task.project_id),
        None,
        "task.deleted",
        &format!("{} deleted ({}, was {}): {reason}", task.short_id(), task.title, task.state),
        actor,
    )?;
    Ok(())
}

/// What deleting a step did.
#[derive(Debug, Default)]
pub struct Deleted {
    pub moved: Vec<String>,
    pub deleted: Vec<String>,
}

/// Deletes a step (or a milestone without steps) for good. Its tasks move to `move_to`, or are
/// deleted with it when `delete_tasks`; with neither, a step that has tasks stays. Everything
/// else that belongs to it (notes, PRs, decisions, timings) is never deleted: it moves to
/// `move_to`, or to the step's milestone.
pub fn delete_step(
    conn: &Connection,
    step: &Milestone,
    move_to: Option<&Milestone>,
    delete_tasks: bool,
    reason: &str,
    actor: &str,
) -> anyhow::Result<Deleted> {
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("say why it's deleted, in reason");
    }
    let all = projects::milestones(conn, step.project_id)?;
    if all.iter().any(|m| m.parent_id == Some(step.id)) {
        bail!("{} has steps; delete or move them first", step.name);
    }
    if let Some(to) = move_to {
        if to.id == step.id || to.project_id != step.project_id {
            bail!("move_to is another step or milestone of the same project");
        }
        if delete_tasks {
            bail!("either move the tasks (move_to) or delete them (delete_tasks), not both");
        }
    }
    let tasks = tasks_of(conn, step.id)?;
    if !tasks.is_empty() && move_to.is_none() && !delete_tasks {
        let list = tasks.iter().map(|t| t.short_id()).collect::<Vec<_>>().join(", ");
        bail!("{} has tasks ({list}): move them with move_to, or delete them too with delete_tasks", step.name);
    }
    let mut out = Deleted::default();
    for t in &tasks {
        match move_to {
            Some(to) => {
                records::update(
                    conn,
                    t,
                    actor,
                    Patch { milestone_id: Some(Some(to.id)), note: format!("moved from {}, deleted", step.name), ..Default::default() },
                )?;
                out.moved.push(t.short_id());
            }
            None => {
                delete_task(conn, t, &format!("with its step {}: {reason}", step.name), actor)?;
                out.deleted.push(t.short_id());
            }
        }
    }
    // The rest goes to move_to, or up to the milestone (or nowhere, for a milestone).
    let home = move_to.map(|m| m.id).or(step.parent_id);
    let others: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM records WHERE milestone_id = ?1")?;
        stmt.query_map([step.id], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    for id in others {
        let r = records::get(conn, id)?;
        records::update(
            conn,
            &r,
            actor,
            Patch { milestone_id: Some(home), note: format!("its step {} was deleted", step.name), quiet: true, ..Default::default() },
        )?;
        out.moved.push(r.short_id());
    }
    conn.execute("UPDATE timings SET milestone_id = ?1 WHERE milestone_id = ?2", params![home, step.id])?;
    conn.execute("DELETE FROM milestones WHERE id = ?1", [step.id])?;
    let what = if step.parent_id.is_some() { "Step" } else { "Milestone" };
    let mut line = format!("{what} {} deleted ({}, {}): {reason}", step.name, step.title, step.state);
    if !out.moved.is_empty() {
        line.push_str(&format!("; moved: {}", out.moved.join(", ")));
    }
    if !out.deleted.is_empty() {
        line.push_str(&format!("; deleted with it: {}", out.deleted.join(", ")));
    }
    event(conn, Some(step.project_id), None, "milestone.deleted", &line, actor)?;
    Ok(out)
}

/// Sets a task blocked. `by` is a question of the project ("Q-12": the task links to it and comes
/// back when it's answered) or a short note for anything internal ("the UX agent's review").
pub fn block(conn: &Connection, task: &Record, by: &str, actor: &str) -> anyhow::Result<Record> {
    let by = by.trim();
    if by.is_empty() {
        bail!("say what blocks it: a question (Q-12) for anything that needs the owner, or a short note");
    }
    let mut data = Map::new();
    let question = match records::RecordRef::parse(by) {
        Ok(r) => records::try_find(conn, &r, Some(&task.project))?.filter(|q| q.kind == Kind::Question),
        Err(_) => None,
    };
    let note = match &question {
        Some(q) => {
            if q.project_id != task.project_id {
                bail!("{} belongs to another project", q.ref_id);
            }
            if q.state != "open" {
                bail!("{} is {} already; nothing to wait for", q.ref_id, q.state);
            }
            records::link(conn, q, task, "blocks", actor)?;
            data.insert("blocked_note".into(), Value::Null);
            format!("blocked by {}", q.ref_id)
        }
        None => {
            if by.chars().count() > 200 {
                bail!("keep the note on what blocks it short (200 characters)");
            }
            data.insert("blocked_note".into(), by.into());
            format!("blocked: {by}")
        }
    };
    let (t, _) = records::update(
        conn,
        task,
        actor,
        Patch { state: Some("blocked".into()), data, note, ..Default::default() },
    )?;
    Ok(t)
}

/// The questions that block a task (open ones only).
pub fn blocking_questions(conn: &Connection, task_id: i64) -> anyhow::Result<Vec<(String, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT r.id FROM links l JOIN records r ON r.id = l.from_id
         WHERE l.to_id = ?1 AND l.relation = 'blocks' AND r.kind = 'question' AND r.state = 'open'",
    )?;
    let ids: Vec<i64> = stmt.query_map([task_id], |r| r.get(0))?.collect::<Result<_, _>>()?;
    ids.into_iter()
        .map(|id| {
            let q = records::get(conn, id)?;
            Ok((q.short_id(), q.title.clone(), q.url()))
        })
        .collect()
}

/// After the owner answered a question: the tasks it blocked go back to todo (unless something
/// else still blocks them), and the agent hears which.
pub fn unblock_after_answer(conn: &Connection, q: &Record, actor: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT l.to_id FROM links l JOIN records r ON r.id = l.to_id
         WHERE l.from_id = ?1 AND l.relation = 'blocks' AND r.kind = 'task' AND r.state = 'blocked'",
    )?;
    let ids: Vec<i64> = stmt.query_map([q.id], |r| r.get(0))?.collect::<Result<_, _>>()?;
    let mut freed = Vec::new();
    for id in ids {
        let t = records::get(conn, id)?;
        if !blocking_questions(conn, t.id)?.is_empty() || !t.str("blocked_note").is_empty() {
            continue;
        }
        records::update(
            conn,
            &t,
            actor,
            Patch {
                state: Some("todo".into()),
                note: format!("unblocked: {} answered", q.short_id()),
                quiet: true,
                ..Default::default()
            },
        )?;
        freed.push(t.short_id());
    }
    if !freed.is_empty() {
        event(
            conn,
            Some(q.project_id),
            Some(q.id),
            "task.unblocked",
            &format!("{} answered: {} unblocked", q.short_id(), freed.join(", ")),
            actor,
        )?;
    }
    Ok(freed)
}

/// Where the agent is, for hello: each step that's under way, with the tasks in doing, the next
/// one, and what's blocked by what.
pub fn where_json(conn: &Connection, project_id: i64) -> anyhow::Result<Value> {
    let ms = projects::milestones(conn, project_id)?;
    let mut tasks = records::query(conn, &Query { project_id: Some(project_id), kinds: vec![Kind::Task], ..Default::default() })?;
    sort_by_position(&mut tasks);
    let mut out = Vec::new();
    for m in ms.iter().filter(|m| m.state == "active") {
        // A milestone with steps is shown through its steps; once they're all concluded, as itself,
        // with what comes next.
        let steps: Vec<&Milestone> = ms.iter().filter(|s| s.parent_id == Some(m.id)).collect();
        if m.parent_id.is_none() && !steps.is_empty() {
            if steps.iter().all(|s| s.state == "closed") {
                out.push(json!({"milestone": m.name, "title": m.title, "doing": [], "next": null, "blocked": [],
                    "hint": "every step is concluded: add the next step, or plan its release and tag (add_release)"}));
            }
            continue;
        }
        let mine: Vec<&Record> = tasks.iter().filter(|t| t.milestone_id == Some(m.id)).collect();
        let doing: Vec<Value> = mine.iter().filter(|t| t.state == "doing").map(|t| json!({"id": t.short_id(), "title": t.title})).collect();
        let next = mine.iter().find(|t| t.state == "todo").map(|t| json!({"id": t.short_id(), "title": t.title, "estimate": t.data.get("estimate")}));
        let mut blocked = Vec::new();
        for t in mine.iter().filter(|t| t.state == "blocked") {
            let qs = blocking_questions(conn, t.id)?;
            let by = if qs.is_empty() { t.str("blocked_note").to_owned() } else { qs.iter().map(|q| q.0.clone()).collect::<Vec<_>>().join(", ") };
            blocked.push(json!({"id": t.short_id(), "title": t.title, "by": by}));
        }
        let open = mine.iter().filter(|t| OPEN.contains(&t.state.as_str())).count();
        let parent = m.parent_id.and_then(|p| ms.iter().find(|x| x.id == p)).map(|p| p.name.clone());
        let mut v = json!({"step": m.name, "title": m.title, "doing": doing, "next": next, "blocked": blocked});
        if let Some(p) = parent {
            v["milestone"] = p.into();
        }
        if open == 0 && !mine.is_empty() {
            v["hint"] = "every task is done or in review: conclude_step when you're through".into();
        }
        out.push(v);
    }
    Ok(Value::Array(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates() {
        assert_eq!(parse_minutes("2 h"), Some(120));
        assert_eq!(parse_minutes("40 min"), Some(40));
        assert_eq!(parse_minutes("1 h 30 min"), Some(90));
        assert_eq!(parse_minutes("1.5h"), Some(90));
        assert_eq!(parse_minutes("90m"), Some(90));
        assert_eq!(parse_minutes("1h30"), None);
        assert_eq!(parse_minutes("soon"), None);
        assert_eq!(parse_minutes(""), None);
        assert_eq!(parse_minutes("0 h"), None);
    }
    use crate::db::Db;
    use crate::records::NewRecord;

    #[test]
    fn steps_start_block_unblock_and_conclude() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = projects::create(c, "lumen", "Lumen", "owner")?;
            let m = projects::add_milestone(c, p.id, &projects::NewMilestone { name: "M3", title: "Albums", description: "", parent: None, before: None }, "lumen")?;
            let s1 = projects::add_milestone(c, p.id, &projects::NewMilestone { name: "S1", title: "Model", description: "", parent: Some(m.id), before: None }, "lumen")?;
            let s2 = projects::add_milestone(c, p.id, &projects::NewMilestone { name: "S2", title: "Sidebar", description: "", parent: Some(m.id), before: None }, "lumen")?;
            let task = |title: &str, mid: i64| {
                records::create(c, p.id, "lumen", NewRecord { kind: Some(Kind::Task), title: title.into(), milestone_id: Some(mid), ..Default::default() })
            };
            let t1 = task("Albums table", s1.id)?;
            let t2 = task("Smart albums", s1.id)?;
            // Doing starts the step and its milestone.
            let (t1, _) = records::update(c, &t1, "lumen", Patch { state: Some("doing".into()), ..Default::default() })?;
            assert_eq!(task_started(c, &t1, "lumen")?, vec!["S1".to_owned(), "M3".to_owned()]);
            assert_eq!(projects::get_milestone(c, s1.id)?.state, "active");
            // A question blocks t2; answering unblocks it.
            let q = records::create(c, p.id, "lumen", NewRecord { kind: Some(Kind::Question), title: "List or grid?".into(), ..Default::default() })?;
            let t2 = block(c, &t2, "Q-1", "lumen")?;
            assert_eq!(t2.state, "blocked");
            assert_eq!(blocking_questions(c, t2.id)?.len(), 1);
            let w = where_json(c, p.id)?;
            assert_eq!(w[0]["step"], "S1");
            assert_eq!(w[0]["blocked"][0]["by"], "Q-1");
            let q = records::answer_question(c, &q, None, "a list", "owner")?;
            assert_eq!(records::get(c, t2.id)?.state, "todo");
            assert!(blocking_questions(c, t2.id)?.is_empty());
            let _ = q;
            // A note blocks without a question; it needs a reason.
            assert!(block(c, &records::get(c, t2.id)?, " ", "lumen").is_err());
            let t2 = block(c, &records::get(c, t2.id)?, "the UX agent's review", "lumen")?;
            assert_eq!(t2.str("blocked_note"), "the UX agent's review");
            // Concluding needs the open tasks dealt with.
            let s1 = projects::get_milestone(c, s1.id)?;
            assert!(conclude(c, &s1, "Albums are in.", None, "lumen").is_err());
            assert!(conclude(c, &s1, " ", None, "lumen").is_err());
            let done = conclude(c, &s1, "Albums are in.\nT-12 turned up.", Some(&s2), "lumen")?;
            assert_eq!(done.step.state, "closed");
            assert_eq!(done.moved.len(), 2);
            assert_eq!(records::get(c, t1.id)?.milestone_id, Some(s2.id));
            assert!(conclude(c, &done.step, "again", None, "lumen").is_err());
            Ok(())
        })
        .unwrap();
    }
}
