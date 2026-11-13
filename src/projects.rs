//! Projects, their agent and their milestones.

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::records::event;
use crate::util::now_ms;

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub description: String,
    pub status: String,
    pub repo: String,
    pub checkout: String,
    pub machine: String,
    pub stack: Vec<String>,
    pub platform: String,
    pub model: String,
    pub effort: String,
    pub pm_rules: Vec<String>,
    pub git_rules: String,
    pub ports: Vec<String>,
    pub build_product: String,
    /// merge, squash or rebase: how its PRs' Merge buttons merge.
    pub merge_method: String,
    pub agent: Agent,
}

/// The project's one agent, as it last introduced itself (`hello`) and when it was last heard from.
#[derive(Debug, Clone, Serialize)]
pub struct Agent {
    pub session: String,
    pub machine: String,
    pub model: String,
    pub effort: String,
    pub note: String,
    pub seen_at: Option<i64>,
}

impl Agent {
    /// Online means heard from within the last 15 minutes.
    pub fn online(&self, now: i64) -> bool {
        self.seen_at.is_some_and(|t| now - t < 15 * 60_000)
    }
}

const PROJECT_COLUMNS: &str = "id, slug, name, description, status, repo, checkout, machine, stack, platform, model, effort, \
     pm_rules, git_rules, ports, build_product, agent_session, agent_machine, agent_model, agent_effort, agent_note, agent_seen_at, \
     merge_method";

fn json_list(s: String) -> Vec<String> {
    serde_json::from_str(&s).unwrap_or_default()
}

fn project_from_row(r: &Row) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get(0)?,
        slug: r.get(1)?,
        name: r.get(2)?,
        description: r.get(3)?,
        status: r.get(4)?,
        repo: r.get(5)?,
        checkout: r.get(6)?,
        machine: r.get(7)?,
        stack: json_list(r.get(8)?),
        platform: r.get(9)?,
        model: r.get(10)?,
        effort: r.get(11)?,
        pm_rules: json_list(r.get(12)?),
        git_rules: r.get(13)?,
        ports: json_list(r.get(14)?),
        build_product: r.get(15)?,
        agent: Agent {
            session: r.get(16)?,
            machine: r.get(17)?,
            model: r.get(18)?,
            effort: r.get(19)?,
            note: r.get(20)?,
            seen_at: r.get(21)?,
        },
        merge_method: r.get(22)?,
    })
}

pub fn list(conn: &Connection) -> anyhow::Result<Vec<Project>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PROJECT_COLUMNS} FROM projects ORDER BY status = 'archived', coalesce(agent_seen_at, 0) DESC, name"
    ))?;
    let rows = stmt.query_map([], project_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn get(conn: &Connection, slug: &str) -> anyhow::Result<Option<Project>> {
    Ok(conn
        .query_row(&format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE slug = ?1"), [slug], project_from_row)
        .optional()?)
}

pub fn get_by_id(conn: &Connection, id: i64) -> anyhow::Result<Project> {
    Ok(conn.query_row(&format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE id = ?1"), [id], project_from_row)?)
}

pub fn require(conn: &Connection, slug: &str) -> anyhow::Result<Project> {
    get(conn, slug)?.with_context(|| format!("no project {slug:?}"))
}

/// The project's details an agent may set. Missing fields stay as they are.
#[derive(Debug, Default, Clone, Deserialize, schemars::JsonSchema)]
pub struct ProjectPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    /// active, paused or archived
    pub status: Option<String>,
    /// GitHub repository as owner/name, e.g. "giosuel/vinculum". Empty until the owner creates it.
    pub repo: Option<String>,
    /// Where the checkout lives, e.g. "~/vinculum".
    pub checkout: Option<String>,
    /// Machine the project is worked on, e.g. "giosuel.ch", "HEADQUARTERS".
    pub machine: Option<String>,
    /// Moved into the project document (2026-11-12); not set through the API any more.
    #[serde(skip)]
    #[schemars(skip)]
    pub stack: Option<Vec<String>>,
    /// Moved into the project document (2026-11-12); not set through the API any more.
    #[serde(skip)]
    #[schemars(skip)]
    pub platform: Option<String>,
    /// Default AI model for the project, e.g. "Claude Opus 5.5".
    pub model: Option<String>,
    /// Default effort, e.g. "high".
    pub effort: Option<String>,
    /// Moved into the project document (2026-11-12); not set through the API any more.
    #[serde(skip)]
    #[schemars(skip)]
    pub pm_rules: Option<Vec<String>>,
    /// Moved into the project document (2026-11-12); not set through the API any more.
    #[serde(skip)]
    #[schemars(skip)]
    pub git_rules: Option<String>,
    /// Ports the project's dev servers use, e.g. ["5173 vite"].
    pub ports: Option<Vec<String>>,
    /// The viber-run product name for test builds.
    pub build_product: Option<String>,
    /// How its PRs merge: merge (a merge commit, the default), squash or rebase. Every PR of the
    /// project's repository gets a Merge button that merges this way and deletes the branch.
    pub merge_method: Option<String>,
}

/// Slugs end up in record ids and URLs: lowercase letters, digits and dashes.
/// A GitHub repository as owner/name: letters, digits, `-`, `_` and `.`.
pub fn looks_like_repo(repo: &str) -> bool {
    let ok = |p: &str| !p.is_empty() && p.len() <= 100 && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    matches!(repo.split_once('/'), Some((o, n)) if ok(o) && ok(n) && !n.contains('/'))
}

pub fn check_slug(slug: &str) -> anyhow::Result<()> {
    let ok = !slug.is_empty()
        && slug.len() <= 40
        && slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-');
    if !ok {
        bail!("{slug:?}: use lowercase letters, digits and dashes, e.g. \"cold-trace\"");
    }
    // Messages are addressed by slug, so these names mean someone else.
    if RESERVED_SLUGS.contains(&slug) {
        bail!("{slug:?} is reserved; pick another slug");
    }
    Ok(())
}

pub const RESERVED_SLUGS: &[&str] = &["all", "owner", "consultant"];

/// The project that publishes builds as `product`: the one whose build_product it is, or the one
/// with that slug if it hasn't set a build_product of its own. Products are unique, so at most one.
pub fn by_build_product(conn: &Connection, product: &str) -> anyhow::Result<Option<Project>> {
    let id: Option<i64> = conn
        .query_row(
            "SELECT id FROM projects WHERE build_product = ?1 OR (build_product = '' AND slug = ?1) ORDER BY id LIMIT 1",
            [product],
            |r| r.get(0),
        )
        .optional()?;
    id.map(|id| get_by_id(conn, id)).transpose()
}

/// A milestone's id and its steps' ids: "tasks of M4" includes those of K1–K4.
pub fn with_steps(all: &[Milestone], id: i64) -> Vec<i64> {
    std::iter::once(id).chain(all.iter().filter(|s| s.parent_id == Some(id)).map(|s| s.id)).collect()
}

pub fn create(conn: &Connection, slug: &str, name: &str, actor: &str) -> anyhow::Result<Project> {
    check_slug(slug)?;
    if get(conn, slug)?.is_some() {
        bail!("project {slug} exists already");
    }
    if by_build_product(conn, slug)?.is_some() {
        bail!("another project publishes builds as {slug:?}; pick another slug");
    }
    let now = now_ms();
    conn.execute(
        "INSERT INTO projects (slug, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
        params![slug, name, now],
    )
    .with_context(|| format!("project {slug} exists already"))?;
    let id = conn.last_insert_rowid();
    event(conn, Some(id), None, "project.created", &format!("Project {name} added"), actor)?;
    // The project's agent reads its feed from here on, whatever it uses first (MCP, the API).
    crate::waiting::cursor(conn, id)?;
    get_by_id(conn, id)
}

/// Applies a patch and returns the names of the fields that changed.
pub fn update(
    conn: &Connection,
    project_id: i64,
    patch: &ProjectPatch,
    actor: &str,
) -> anyhow::Result<Vec<&'static str>> {
    let mut changed = Vec::new();
    let mut set = |col: &'static str, value: String| -> anyhow::Result<()> {
        let n = conn.execute(
            &format!("UPDATE projects SET {col} = ?1 WHERE id = ?2 AND {col} IS NOT ?1"),
            params![value, project_id],
        )?;
        if n > 0 {
            changed.push(col);
        }
        Ok(())
    };
    if let Some(v) = &patch.status
        && !["active", "paused", "archived"].contains(&v.as_str())
    {
        bail!("status must be active, paused or archived");
    }
    if let Some(v) = patch.build_product.as_deref().map(str::trim) {
        // Empty means "builds under the slug", which must be free too.
        let product = if v.is_empty() { get_by_id(conn, project_id)?.slug } else { v.to_owned() };
        crate::builds::check_product(&product)?;
        let taken: Option<i64> = conn
            .query_row(
                "SELECT id FROM projects WHERE id != ?2 AND (build_product = ?1 OR (build_product = '' AND slug = ?1))",
                params![product, project_id],
                |r| r.get(0),
            )
            .optional()?;
        if taken.is_some() {
            bail!("another project publishes builds as {product:?} already");
        }
    }
    if let Some(v) = &patch.merge_method {
        if !["merge", "squash", "rebase"].contains(&v.trim()) {
            bail!("merge_method is merge, squash or rebase");
        }
        set("merge_method", v.trim().into())?;
        // Waiting Merge buttons AgentFlow made follow the new method.
        let p = get_by_id(conn, project_id)?;
        let mut stmt = conn.prepare(
            "SELECT a.id, r.number FROM actions a JOIN records r ON r.id = a.record_id
             WHERE r.project_id = ?1 AND r.kind = 'pr' AND a.kind = 'merge' AND a.added_by = 'agentflow' AND a.state = 'waiting'",
        )?;
        let rows: Vec<(i64, i64)> = stmt.query_map([project_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        for (id, number) in rows {
            conn.execute(
                "UPDATE actions SET command = ?1 WHERE id = ?2",
                params![crate::prs::merge_command(p.repo.trim(), number, v.trim()), id],
            )?;
        }
    }
    if let Some(v) = &patch.name {
        if v.trim().is_empty() {
            bail!("the name can't be empty");
        }
        set("name", v.trim().into())?;
    }
    for (col, v) in [
        ("description", &patch.description),
        ("status", &patch.status),
        ("repo", &patch.repo),
        ("checkout", &patch.checkout),
        ("machine", &patch.machine),
        ("platform", &patch.platform),
        ("model", &patch.model),
        ("effort", &patch.effort),
        ("git_rules", &patch.git_rules),
        ("build_product", &patch.build_product),
    ] {
        if let Some(v) = v {
            set(col, v.trim().into())?;
        }
    }
    for (col, v) in [("stack", &patch.stack), ("pm_rules", &patch.pm_rules), ("ports", &patch.ports)] {
        if let Some(v) = v {
            let clean: Vec<&str> = v.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            set(col, serde_json::to_string(&clean)?)?;
        }
    }
    if !changed.is_empty() {
        conn.execute("UPDATE projects SET updated_at = ?1 WHERE id = ?2", params![now_ms(), project_id])?;
        event(
            conn,
            Some(project_id),
            None,
            "project.updated",
            &format!("Project details changed: {}", changed.join(", ")),
            actor,
        )?;
    }
    Ok(changed)
}

/// The agent introduces itself at the start of a session.
/// Returns when the agent said hello before (None the first time).
pub fn hello(
    conn: &Connection,
    project_id: i64,
    session: &str,
    machine: &str,
    model: &str,
    effort: &str,
) -> anyhow::Result<Option<i64>> {
    let prev: Option<i64> = conn.query_row("SELECT agent_hello_at FROM projects WHERE id = ?1", [project_id], |r| r.get(0))?;
    conn.execute("UPDATE projects SET agent_hello_at = ?1 WHERE id = ?2", params![now_ms(), project_id])?;
    conn.execute(
        // A new session starts without the last one's "working on" note (it'd be stale).
        "UPDATE projects SET agent_session = ?1, agent_machine = ?2, agent_model = ?3, agent_effort = ?4, agent_seen_at = ?5,
         agent_note = '' WHERE id = ?6",
        params![session.trim(), machine.trim(), model.trim(), effort.trim(), now_ms(), project_id],
    )?;
    Ok(prev)
}

/// Every call from a project's token counts as a sign of life.
pub fn seen(conn: &Connection, project_id: i64) -> anyhow::Result<()> {
    conn.execute("UPDATE projects SET agent_seen_at = ?1 WHERE id = ?2", params![now_ms(), project_id])?;
    Ok(())
}

/// What the agent says it's doing right now, shown on the project page.
pub fn set_agent_note(conn: &Connection, project_id: i64, note: &str) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE projects SET agent_note = ?1 WHERE id = ?2",
        params![crate::activity::redact(note.trim()), project_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Milestones

#[derive(Debug, Clone, Serialize)]
pub struct Milestone {
    pub id: i64,
    pub project_id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub title: String,
    pub description: String,
    pub position: f64,
    pub state: String,
    pub tag: String,
    pub reopened_because: String,
    pub closed_at: Option<i64>,
    /// When its first task went to doing (or it was set active).
    pub started_at: Option<i64>,
    /// A concluded step's summary, in the agent's words.
    pub summary: String,
    /// A review step: reviewers, commit, the owner's words, reports (JSON); empty otherwise.
    pub review: String,
    /// Archived (hidden from the lists, kept, restorable), and why.
    pub archived_at: Option<i64>,
    pub archived_reason: String,
}

const MILESTONE_COLUMNS: &str =
    "id, project_id, parent_id, name, title, description, position, state, tag, reopened_because, closed_at, started_at, summary, review, archived_at, archived_reason";

fn milestone_from_row(r: &Row) -> rusqlite::Result<Milestone> {
    Ok(Milestone {
        id: r.get(0)?,
        project_id: r.get(1)?,
        parent_id: r.get(2)?,
        name: r.get(3)?,
        title: r.get(4)?,
        description: r.get(5)?,
        position: r.get(6)?,
        state: r.get(7)?,
        tag: r.get(8)?,
        reopened_because: r.get(9)?,
        closed_at: r.get(10)?,
        started_at: r.get(11)?,
        summary: r.get(12)?,
        review: r.get(13)?,
        archived_at: r.get(14)?,
        archived_reason: r.get(15)?,
    })
}

/// All milestones and steps of a project, in order (steps after their milestone's siblings sort).
/// Archived ones are left out.
pub fn milestones(conn: &Connection, project_id: i64) -> anyhow::Result<Vec<Milestone>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MILESTONE_COLUMNS} FROM milestones WHERE project_id = ?1 AND archived_at IS NULL ORDER BY position, id"
    ))?;
    let rows = stmt.query_map([project_id], milestone_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Archived milestones and steps, in order.
pub fn archived_milestones(conn: &Connection, project_id: i64) -> anyhow::Result<Vec<Milestone>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MILESTONE_COLUMNS} FROM milestones WHERE project_id = ?1 AND archived_at IS NOT NULL ORDER BY position, id"
    ))?;
    let rows = stmt.query_map([project_id], milestone_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Finds a milestone or step by name among the live ones, or among the archived ones too.
pub fn find_milestone_any(conn: &Connection, project_id: i64, name: &str, archived: bool) -> anyhow::Result<Milestone> {
    match find_milestone(conn, project_id, name) {
        Ok(m) => Ok(m),
        Err(e) if !archived => Err(e),
        Err(e) => find_in(&archived_milestones(conn, project_id)?, conn, name).map_err(|_| e),
    }
}

fn find_in(all: &[Milestone], conn: &Connection, name: &str) -> anyhow::Result<Milestone> {
    let name = name.trim();
    let (parent, step) = match name.split_once('/') {
        Some((p, s)) => (Some(p), s),
        None => (None, name),
    };
    let hits: Vec<&Milestone> = all
        .iter()
        .filter(|m| m.name.eq_ignore_ascii_case(step))
        .filter(|m| match parent {
            None => true,
            Some(p) => m.parent_id.and_then(|id| get_milestone(conn, id).ok()).is_some_and(|pm| pm.name.eq_ignore_ascii_case(p)),
        })
        .collect();
    match hits.as_slice() {
        [m] => Ok((*m).clone()),
        [] => bail!("no milestone {name:?}"),
        _ => bail!("{name:?} names more than one; write it as MILESTONE/STEP"),
    }
}

/// The scope a question or decision lands in when none is given (the owner, 2026-11-12: "the
/// default being the current milestone"): the active milestone; with several, the one whose step
/// has a task in doing, otherwise the one started last. None (project-wide) when none is active.
pub fn default_scope(conn: &Connection, project_id: i64) -> anyhow::Result<Option<Milestone>> {
    let all = milestones(conn, project_id)?;
    let active: Vec<&Milestone> = all.iter().filter(|m| m.parent_id.is_none() && m.state == "active").collect();
    if active.len() <= 1 {
        return Ok(active.first().map(|m| (*m).clone()));
    }
    let doing: Vec<Option<i64>> = {
        let mut stmt = conn.prepare("SELECT milestone_id FROM records WHERE project_id = ?1 AND kind = 'task' AND state = 'doing'")?;
        stmt.query_map([project_id], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    for id in doing.into_iter().flatten() {
        if let Some(m) = all.iter().find(|m| m.id == id) {
            let top = m.parent_id.unwrap_or(m.id);
            if let Some(a) = active.iter().find(|a| a.id == top) {
                return Ok(Some((*a).clone()));
            }
        }
    }
    Ok(active.iter().max_by_key(|m| (m.started_at.unwrap_or(0), m.id)).map(|m| (*m).clone()))
}

/// A scope as an agent writes it: "project", a milestone ("M9"), or a step ("P9.3", meaning its
/// milestone). None or empty: the default scope. Returns the milestone, or None for project-wide.
pub fn scope(conn: &Connection, project_id: i64, raw: Option<&str>) -> anyhow::Result<Option<Milestone>> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => default_scope(conn, project_id),
        Some(s) if s.eq_ignore_ascii_case("project") => Ok(None),
        Some(s) => {
            let m = find_milestone(conn, project_id, s)
                .map_err(|e| anyhow::anyhow!("scope is \"project\" or a milestone, e.g. \"M9\": {e:#}"))?;
            Ok(Some(match m.parent_id {
                Some(p) => get_milestone(conn, p)?,
                None => m,
            }))
        }
    }
}

/// How a scope reads: "M9" or "project".
pub fn scope_name(conn: &Connection, milestone_id: Option<i64>) -> String {
    milestone_id.and_then(|id| get_milestone(conn, id).ok()).map(|m| m.name).unwrap_or_else(|| "project".into())
}

/// Finds a milestone or step by name ("M4", or "K2" for a step; "M4/K2" when a step name repeats).
pub fn find_milestone(conn: &Connection, project_id: i64, name: &str) -> anyhow::Result<Milestone> {
    let all = milestones(conn, project_id)?;
    let name = name.trim();
    if let Some((parent, step)) = name.split_once('/') {
        let p = all.iter().find(|m| m.parent_id.is_none() && m.name.eq_ignore_ascii_case(parent));
        if let Some(p) = p
            && let Some(m) = all.iter().find(|m| m.parent_id == Some(p.id) && m.name.eq_ignore_ascii_case(step))
        {
            return Ok(m.clone());
        }
        bail!("no milestone {name:?}");
    }
    let mut hits = all.iter().filter(|m| m.name.eq_ignore_ascii_case(name));
    match (hits.next(), hits.next()) {
        (Some(m), None) => Ok(m.clone()),
        (Some(_), Some(_)) => bail!("{name:?} names more than one step; write it as MILESTONE/STEP, e.g. M4/{name}"),
        (None, _) => {
            if let Ok(m) = find_in(&archived_milestones(conn, project_id)?, conn, name) {
                bail!("{} is archived; restore_milestone brings it back", m.name);
            }
            bail!("no milestone {name:?}")
        }
    }
}

/// Makes a step a review step.
pub fn set_review(conn: &Connection, m: &Milestone, review: &serde_json::Value) -> anyhow::Result<()> {
    conn.execute("UPDATE milestones SET review = ?1 WHERE id = ?2", params![review.to_string(), m.id])?;
    Ok(())
}

pub fn get_milestone(conn: &Connection, id: i64) -> anyhow::Result<Milestone> {
    Ok(conn.query_row(
        &format!("SELECT {MILESTONE_COLUMNS} FROM milestones WHERE id = ?1"),
        [id],
        milestone_from_row,
    )?)
}

pub struct NewMilestone<'a> {
    pub name: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    pub parent: Option<i64>,
    /// Insert before this milestone; at the end when None.
    pub before: Option<i64>,
}

pub fn add_milestone(conn: &Connection, project_id: i64, m: &NewMilestone, actor: &str) -> anyhow::Result<Milestone> {
    let name = m.name.trim();
    if name.is_empty() || name.len() > 20 || name.contains('/') {
        bail!("a milestone name is short, without '/', e.g. M4, A3, P5 or K2");
    }
    if m.title.trim().is_empty() {
        bail!("a milestone needs a title");
    }
    if let Some(parent) = m.parent {
        let p = get_milestone(conn, parent)?;
        if p.project_id != project_id || p.parent_id.is_some() {
            bail!("steps belong to a milestone of the same project, one level deep");
        }
    }
    let position = position_before(conn, project_id, m.before)?;
    let now = now_ms();
    conn.execute(
        "INSERT INTO milestones (project_id, parent_id, name, title, description, position, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        params![project_id, m.parent, name, m.title.trim(), m.description.trim(), position, now],
    )
    .with_context(|| format!("{name} exists already (maybe archived: names stay taken; rename one of them)"))?;
    let id = conn.last_insert_rowid();
    let what = if m.parent.is_some() { "Step" } else { "Milestone" };
    event(conn, Some(project_id), None, "milestone.added", &format!("{what} {name} added: {}", m.title.trim()), actor)?;
    get_milestone(conn, id)
}

/// A position that sorts just before `before` (or after everything).
fn position_before(conn: &Connection, project_id: i64, before: Option<i64>) -> anyhow::Result<f64> {
    match before {
        None => {
            let max: Option<f64> =
                conn.query_row("SELECT max(position) FROM milestones WHERE project_id = ?1", [project_id], |r| {
                    r.get(0)
                })?;
            Ok(max.unwrap_or(0.0) + 1.0)
        }
        Some(id) => {
            let target: f64 = conn
                .query_row(
                    "SELECT position FROM milestones WHERE id = ?1 AND project_id = ?2",
                    params![id, project_id],
                    |r| r.get(0),
                )
                .context("no such milestone to insert before")?;
            let prev: Option<f64> = conn.query_row(
                "SELECT max(position) FROM milestones WHERE project_id = ?1 AND position < ?2",
                params![project_id, target],
                |r| r.get(0),
            )?;
            Ok(match prev {
                Some(p) => (p + target) / 2.0,
                None => target - 1.0,
            })
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize, schemars::JsonSchema)]
pub struct MilestonePatch {
    pub name: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    /// planned, active or closed.
    pub state: Option<String>,
    /// The tag that closed it, e.g. "v0.0.5-m4".
    pub tag: Option<String>,
    /// Why a closed milestone or step is reopened, if there's something worth saying.
    pub reopened_because: Option<String>,
    /// Move it before this milestone (by name); "end" moves it to the end.
    pub move_before: Option<String>,
    /// When it really started and ended, e.g. "2026-10-14" or "2026-10-14 09:30" (UTC): for
    /// history brought in from elsewhere (an import).
    pub started_at: Option<String>,
    pub closed_at: Option<String>,
    /// A step's summary: rewrite it, e.g. when tasks were moved in from other steps (conclude_step
    /// writes it for steps concluded here). Empty removes it.
    pub summary: Option<String>,
    /// Move a step to another milestone, e.g. "P10".
    pub step_of: Option<String>,
}

pub fn update_milestone(
    conn: &Connection,
    m: &Milestone,
    patch: &MilestonePatch,
    actor: &str,
) -> anyhow::Result<Milestone> {
    let mut notes = Vec::new();
    let now = now_ms();
    if let Some(state) = &patch.state
        && state != &m.state
    {
        if !["planned", "active", "closed"].contains(&state.as_str()) {
            bail!("state must be planned, active or closed");
        }
        if m.state == "closed" {
            // Milestones and steps reopen freely (the owner, 2026-11-12); the event log keeps it.
            let why = patch.reopened_because.as_deref().map(str::trim).unwrap_or("");
            conn.execute(
                "UPDATE milestones SET reopened_because = ?1, closed_at = NULL WHERE id = ?2",
                params![why, m.id],
            )?;
            notes.push(if why.is_empty() { "reopened".into() } else { format!("reopened: {why}") });
        }
        if state == "closed" {
            conn.execute("UPDATE milestones SET closed_at = ?1 WHERE id = ?2", params![now, m.id])?;
        }
        if state == "active" {
            conn.execute("UPDATE milestones SET started_at = coalesce(started_at, ?1) WHERE id = ?2", params![now, m.id])?;
        }
        conn.execute("UPDATE milestones SET state = ?1 WHERE id = ?2", params![state, m.id])?;
        notes.push(format!("now {state}"));
    }
    for (col, v) in
        [("name", &patch.name), ("title", &patch.title), ("description", &patch.description), ("tag", &patch.tag)]
    {
        if let Some(v) = v {
            if (col == "name" || col == "title") && v.trim().is_empty() {
                bail!("{col} can't be empty");
            }
            let n = conn.execute(
                &format!("UPDATE milestones SET {col} = ?1 WHERE id = ?2 AND {col} != ?1"),
                params![v.trim(), m.id],
            )?;
            if n > 0 {
                notes.push(format!("{col} changed"));
            }
        }
    }
    for (col, v) in [("started_at", &patch.started_at), ("closed_at", &patch.closed_at)] {
        if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            let at = crate::util::parse_day_or_time(v)?;
            if at > now {
                bail!("{col} is in the future");
            }
            if col == "closed_at" && m.state != "closed" && patch.state.as_deref() != Some("closed") {
                bail!("closed_at is for a closed milestone (state closed)");
            }
            conn.execute(&format!("UPDATE milestones SET {col} = ?1 WHERE id = ?2"), params![at, m.id])?;
            notes.push(format!("{col} set to {v}"));
        }
    }
    if let Some(sum) = patch.summary.as_deref().map(str::trim) {
        let n = conn.execute(
            "UPDATE milestones SET summary = ?1 WHERE id = ?2 AND summary != ?1",
            params![crate::activity::redact(sum), m.id],
        )?;
        if n > 0 {
            notes.push(if sum.is_empty() { "summary removed".into() } else { "summary set".into() });
        }
    }
    if let Some(parent) = patch.step_of.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        let p = find_milestone(conn, m.project_id, parent)?;
        if m.parent_id.is_none() {
            bail!("{} is a milestone; only steps move to another milestone", m.name);
        }
        if p.parent_id.is_some() {
            bail!("{} is a step; steps belong to a milestone", p.name);
        }
        if m.parent_id != Some(p.id) {
            conn.execute("UPDATE milestones SET parent_id = ?1 WHERE id = ?2", params![p.id, m.id])
                .with_context(|| format!("{} has a step {} already", p.name, m.name))?;
            notes.push(format!("now a step of {}", p.name));
        }
    }
    if let Some(before) = &patch.move_before {
        let pos = if before.eq_ignore_ascii_case("end") {
            position_before(conn, m.project_id, None)?
        } else {
            let target = find_milestone(conn, m.project_id, before)?;
            position_before(conn, m.project_id, Some(target.id))?
        };
        conn.execute("UPDATE milestones SET position = ?1 WHERE id = ?2", params![pos, m.id])?;
        notes.push(format!("moved before {before}"));
    }
    if !notes.is_empty() {
        conn.execute("UPDATE milestones SET updated_at = ?1 WHERE id = ?2", params![now, m.id])?;
        event(
            conn,
            Some(m.project_id),
            None,
            "milestone.updated",
            &format!("{}: {}", m.name, notes.join(", ")),
            actor,
        )?;
    }
    get_milestone(conn, m.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn slugs() {
        for ok in ["vinculum", "cold-trace", "p2"] {
            assert!(check_slug(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Cold", "a b", "-x", "a/b", "ä", "all", "owner", "consultant"] {
            assert!(check_slug(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn projects_and_milestones() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = create(c, "vinculum", "Vinculum", "admin")?;
            assert!(create(c, "vinculum", "again", "admin").is_err());
            for reserved in RESERVED_SLUGS {
                assert!(create(c, reserved, "x", "admin").is_err(), "{reserved}");
            }
            // A product is one project's: nobody publishes as another's slug or product.
            let i = create(c, "imaginarium", "Imaginarium", "admin")?;
            let product = |v: &str| ProjectPatch { build_product: Some(v.into()), ..Default::default() };
            assert!(update(c, p.id, &product("imaginarium"), "admin").is_err());
            assert!(update(c, p.id, &product("Bad Name"), "admin").is_err());
            update(c, i.id, &product("imag"), "admin")?;
            assert!(update(c, p.id, &product("imag"), "admin").is_err());
            assert!(create(c, "imag", "x", "admin").is_err());
            assert_eq!(by_build_product(c, "imag")?.map(|x| x.id), Some(i.id));
            assert!(by_build_product(c, "imaginarium")?.is_none(), "it publishes as imag now");
            let changed = update(
                c,
                p.id,
                &ProjectPatch {
                    stack: Some(vec!["Rust".into(), " Slint ".into(), "".into()]),
                    status: None,
                    ..Default::default()
                },
                "vinculum",
            )?;
            assert_eq!(changed, vec!["stack"]);
            assert_eq!(get(c, "vinculum")?.unwrap().stack, vec!["Rust", "Slint"]);
            assert!(update(c, p.id, &ProjectPatch { status: Some("gone".into()), ..Default::default() }, "x").is_err());

            let m = |name, before| NewMilestone { name, title: "t", description: "", parent: None, before };
            let m3 = add_milestone(c, p.id, &m("M3", None), "a")?;
            let m4 = add_milestone(c, p.id, &m("M4", None), "a")?;
            let p4 = add_milestone(c, p.id, &m("P4", Some(m4.id)), "a")?;
            let names: Vec<_> = milestones(c, p.id)?.into_iter().map(|m| m.name).collect();
            assert_eq!(names, ["M3", "P4", "M4"]);
            assert!(add_milestone(c, p.id, &m("M4", None), "a").is_err());

            let k1 = add_milestone(
                c,
                p.id,
                &NewMilestone { name: "K1", title: "k", description: "", parent: Some(m4.id), before: None },
                "a",
            )?;
            assert_eq!(find_milestone(c, p.id, "k1")?.id, k1.id);
            assert_eq!(find_milestone(c, p.id, "M4/K1")?.id, k1.id);

            let closed = update_milestone(
                c,
                &m3,
                &MilestonePatch { state: Some("closed".into()), tag: Some("v0.0.4-m3".into()), ..Default::default() },
                "a",
            )?;
            assert_eq!(closed.state, "closed");
            // Reopening needs no reason any more (the owner, 2026-11-12: agents edit freely).
            let open = update_milestone(c, &closed, &MilestonePatch { state: Some("active".into()), ..Default::default() }, "a")?;
            assert_eq!(open.state, "active");
            let closed = update_milestone(c, &open, &MilestonePatch { state: Some("closed".into()), ..Default::default() }, "a")?;
            let reopened = update_milestone(
                c,
                &closed,
                &MilestonePatch {
                    state: Some("active".into()),
                    reopened_because: Some("missed a fix".into()),
                    ..Default::default()
                },
                "a",
            )?;
            assert_eq!(reopened.reopened_because, "missed a fix");

            update_milestone(c, &p4, &MilestonePatch { move_before: Some("end".into()), ..Default::default() }, "a")?;
            let names: Vec<_> =
                milestones(c, p.id)?.into_iter().filter(|m| m.parent_id.is_none()).map(|m| m.name).collect();
            assert_eq!(names, ["M3", "M4", "P4"]);
            Ok(())
        })
        .unwrap();
    }
}
