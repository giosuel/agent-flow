//! Drafts for other people's repositories: new PRs and issues, comments on them, and new commits
//! for a PR already sent. Agents (the consultant too) only draft; the owner reads the draft on its
//! page, may edit the text, and presses Send. Only then does AgentFlow push and post, as the owner,
//! with exactly the text the owner sent.
//!
//! The owner, 2026-11-11: "every external PR / comment has to go through me. this is important.
//! agents, including the consultant, are not allowed to push to external repos (even if the github
//! token allows it)".
//!
//! A draft is a Contribution record. A new PR or issue is the record itself while it's a draft
//! (`shown`: waiting for the owner; `draft`: back with the agent after "Ask for changes"); once sent
//! it's watched like any other (see `contributions`). A comment or new commits for one already sent
//! sit in its `reply` until sent. The agent's commits arrive as a git bundle; AgentFlow reads them
//! into a repository of its own and shows the diff it computed itself, so what the owner reviews is
//! what gets pushed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::records::{self, Kind, NewRecord, Patch, Record, RecordRef};

/// The biggest bundle of commits an agent can send.
pub const MAX_BUNDLE: usize = 20 * 1024 * 1024;
/// The biggest diff shown for review; a bigger change is too big to review here.
pub const MAX_DIFF: usize = 4 * 1024 * 1024;
/// At most this many commits in one draft.
pub const MAX_COMMITS: usize = 100;
/// GitHub's own limit for a PR's, issue's or comment's text.
pub const MAX_TEXT: usize = 65_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftType {
    /// A new pull request: commits on top of their branch.
    Pr,
    /// A new issue.
    Issue,
    /// A comment on a PR or issue already sent.
    Comment,
    /// New commits for a PR already sent (pushed on top of its branch, never forced).
    Commits,
}

impl DraftType {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s.trim() {
            "pr" => Self::Pr,
            "issue" => Self::Issue,
            "comment" => Self::Comment,
            "commits" => Self::Commits,
            _ => bail!("the type is pr, issue, comment or commits"),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pr => "pr",
            Self::Issue => "issue",
            Self::Comment => "comment",
            Self::Commits => "commits",
        }
    }

    /// A new PR or issue, rather than something for one already sent.
    pub fn is_new(self) -> bool {
        matches!(self, Self::Pr | Self::Issue)
    }

    pub fn has_code(self) -> bool {
        matches!(self, Self::Pr | Self::Commits)
    }

    /// "the PR", "the issue", "the comment", "the commits": the Send button's words.
    pub fn noun(self) -> &'static str {
        match self {
            Self::Pr => "the PR",
            Self::Issue => "the issue",
            Self::Comment => "the comment",
            Self::Commits => "the commits",
        }
    }
}

/// What an agent sends: a new draft, or a new version of one.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DraftIn {
    #[serde(rename = "type")]
    pub kind: String,
    /// Theirs, e.g. "ophymx/formulary" (new PRs and issues).
    #[serde(default)]
    pub repo: String,
    /// The branch of theirs a new PR goes into, e.g. "main".
    #[serde(default)]
    pub base: String,
    /// The branch name on the owner's fork for a new PR, e.g. "center-fences".
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub title: String,
    /// The text for them (Markdown).
    #[serde(default)]
    pub body: String,
    /// For the owner only: why, in a sentence or two.
    #[serde(default)]
    pub why: String,
    /// The project's task it's for, e.g. T-88.
    #[serde(default)]
    pub task: String,
    /// The owner's decision that agreed to it, e.g. D-31.
    #[serde(default)]
    pub agreed: String,
    /// The contribution a comment or commits are for, e.g. C-4.
    #[serde(default)]
    pub on: String,
    /// A draft to replace with this version, e.g. C-7 (for a comment or commits: the same C-4).
    #[serde(default)]
    pub revise: String,
    /// The commits, as `git bundle create - <their branch>..HEAD`, base64.
    #[serde(default)]
    pub bundle: String,
    /// Only for tokens that may write to more than one project (the consultant).
    #[serde(default)]
    pub project: Option<String>,
}

/// A safe branch name: letters, digits, `.`, `_`, `-`, `/`, not starting with `-` or `/`.
pub fn check_branch(b: &str) -> anyhow::Result<()> {
    let ok = !b.is_empty()
        && b.len() <= 100
        && b.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-/".contains(&c))
        && !b.starts_with(['-', '/', '.'])
        && !b.ends_with(['/', '.'])
        && !b.contains("..")
        && !b.contains("//")
        && !b.ends_with(".lock");
    if !ok {
        bail!("{b:?} isn't a branch name AgentFlow uses (letters, digits, . _ - /)");
    }
    Ok(())
}

/// The text for them; only new commits may come without one (it becomes a comment if there is).
fn check_text(body: &str, required: bool) -> anyhow::Result<()> {
    if required && body.trim().is_empty() {
        bail!("the text is empty");
    }
    if body.chars().count() > MAX_TEXT {
        bail!("the text is longer than {MAX_TEXT} characters");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Git, in AgentFlow's own repositories (one bare repository per repository of theirs)

/// Runs git with a timeout; the output, or the error with git's own words. `secret` (a token) is
/// passed in the environment only, and blanked from anything git prints.
async fn git(dir: &Path, args: &[&str], secret: Option<&str>) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    cmd.env("GIT_TERMINAL_PROMPT", "0").env("GIT_ASKPASS", "true").env_remove("GH_TOKEN").kill_on_drop(true);
    if let Some(s) = secret {
        cmd.env("AF_PUSH_TOKEN", s);
    }
    let out = tokio::time::timeout(Duration::from_secs(300), cmd.output())
        .await
        .context("git took longer than 5 minutes")?
        .context("running git")?;
    let blank = |s: String| match secret {
        Some(t) if !t.is_empty() => s.replace(t, "…"),
        _ => s,
    };
    if !out.status.success() {
        let err = blank(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        bail!("git {}: {}", args.first().unwrap_or(&""), err.lines().last().unwrap_or("failed"));
    }
    Ok(blank(String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// AgentFlow's repository for one of theirs.
fn cache_dir(data_dir: &Path, repo: &str) -> PathBuf {
    data_dir.join("outgoing").join("git").join(format!("{}.git", repo.replace('/', "__")))
}

async fn cache_repo(data_dir: &Path, repo: &str) -> anyhow::Result<PathBuf> {
    let dir = cache_dir(data_dir, repo);
    if !dir.join("HEAD").exists() {
        tokio::fs::create_dir_all(&dir).await?;
        git(&dir, &["init", "--bare", "-q"], None).await?;
    }
    Ok(dir)
}

/// The commits and diff of a draft, as AgentFlow read them from the bundle.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Code {
    /// Where the commits are in AgentFlow's repository.
    pub git_ref: String,
    pub sha: String,
    /// What they go on top of (their branch, or the PR's branch on the fork), and its commit.
    pub base_ref: String,
    pub base_sha: String,
    pub commits: Vec<Commit>,
    /// "2 files changed, 12 insertions(+), 3 deletions(-)"
    pub stat: String,
    /// The diff's file, in the data directory.
    pub diff_file: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub author: String,
    pub subject: String,
    pub body: String,
}

/// Fetches `branch` of `from` (theirs, or the owner's fork) into AgentFlow's repository for `repo`,
/// reads the bundle's commits on top of it into `git_ref`, and works out what the owner reviews.
/// For new commits on a PR already sent (`fast_forward`), they must build on the branch as it is.
#[allow(clippy::too_many_arguments)]
async fn import(
    data_dir: &Path,
    git_host: &str,
    repo: &str,
    from: &str,
    branch: &str,
    bundle: &[u8],
    git_ref: &str,
    diff_file: &Path,
    fast_forward: bool,
) -> anyhow::Result<Code> {
    let dir = cache_repo(data_dir, repo).await?;
    let base_ref = format!("refs/remote/{}/{branch}", from.replace('/', "__"));
    let url = format!("{}/{from}.git", git_host.trim_end_matches('/'));
    git(&dir, &["fetch", "-q", "--no-tags", &url, &format!("+refs/heads/{branch}:{base_ref}")], None)
        .await
        .with_context(|| format!("fetching {from}'s {branch}"))?;
    let tmp_dir = data_dir.join("outgoing").join("tmp");
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let tmp = tempfile::Builder::new().suffix(".bundle").tempfile_in(&tmp_dir)?;
    tokio::fs::write(tmp.path(), bundle).await?;
    let path = tmp.path().to_string_lossy().to_string();
    git(&dir, &["bundle", "verify", "-q", &path], None)
        .await
        .context("the bundle doesn't fit their branch: make it with `git bundle create <file> <their branch>..HEAD`")?;
    git(&dir, &["fetch", "-q", "--no-tags", &path, &format!("+HEAD:{git_ref}")], None).await?;
    let sha = git(&dir, &["rev-parse", "--verify", &format!("{git_ref}^{{commit}}")], None).await?.trim().to_owned();
    let base_sha =
        git(&dir, &["rev-parse", "--verify", &format!("{base_ref}^{{commit}}")], None).await?.trim().to_owned();
    if fast_forward && git(&dir, &["merge-base", "--is-ancestor", &base_sha, &sha], None).await.is_err() {
        bail!(
            "the commits don't build on the PR's branch as it is now: fetch it, put them on top, and send again (AgentFlow never force-pushes)"
        );
    }
    let range = format!("{base_sha}..{sha}");
    let log = git(
        &dir,
        &["log", &format!("-n{}", MAX_COMMITS + 1), "--reverse", "--format=%H%x1f%an <%ae>%x1f%s%x1f%b%x1e", &range],
        None,
    )
    .await?;
    let commits: Vec<Commit> = log
        .split('\u{1e}')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|c| {
            let mut f = c.split('\u{1f}');
            Some(Commit {
                sha: f.next()?.to_owned(),
                author: f.next()?.to_owned(),
                subject: f.next()?.to_owned(),
                body: f.next().unwrap_or("").trim().to_owned(),
            })
        })
        .collect();
    if commits.is_empty() {
        bail!("there are no commits on top of {from}'s {branch}");
    }
    if commits.len() > MAX_COMMITS {
        bail!("more than {MAX_COMMITS} commits; that's too many to review here");
    }
    // A new PR shows what it changes against where it branched off; new commits, what they add.
    let diff_range = if fast_forward { range.clone() } else { format!("{base_sha}...{sha}") };
    let stat = git(&dir, &["diff", "--shortstat", &diff_range], None).await?.trim().to_owned();
    let diff = git(&dir, &["diff", "--no-color", "--no-ext-diff", "--find-renames", &diff_range], None).await?;
    if diff.len() > MAX_DIFF {
        bail!("the diff is bigger than {} MB; that's too big to review here", MAX_DIFF / 1024 / 1024);
    }
    if let Some(p) = diff_file.parent() {
        tokio::fs::create_dir_all(p).await?;
    }
    tokio::fs::write(diff_file, &diff).await?;
    Ok(Code {
        git_ref: git_ref.to_owned(),
        sha,
        base_ref,
        base_sha,
        commits,
        stat,
        diff_file: diff_file.to_string_lossy().to_string(),
    })
}

/// A file of theirs at the commit the draft builds on, e.g. their PR template.
async fn their_file(data_dir: &Path, repo: &str, base_ref: &str, names: &[&str]) -> Option<(String, String)> {
    let dir = cache_dir(data_dir, repo);
    for name in names {
        if let Ok(text) = git(&dir, &["show", &format!("{base_ref}:{name}")], None).await
            && text.len() < 64 * 1024
        {
            return Some(((*name).to_owned(), text));
        }
    }
    None
}

const PR_TEMPLATES: &[&str] = &[
    ".github/pull_request_template.md",
    ".github/PULL_REQUEST_TEMPLATE.md",
    "pull_request_template.md",
    "PULL_REQUEST_TEMPLATE.md",
    "docs/pull_request_template.md",
    "docs/PULL_REQUEST_TEMPLATE.md",
];
const CONTRIBUTING: &[&str] = &["CONTRIBUTING.md", ".github/CONTRIBUTING.md", "docs/CONTRIBUTING.md"];

// ---------------------------------------------------------------------------------------------
// Drafting (agents and the consultant)

/// What `submit` needs from the server.
pub struct Env<'a> {
    pub data_dir: &'a Path,
    pub git_host: &'a str,
}

/// What the database step of `submit` decided, before the git step.
struct Plan {
    project_id: i64,
    slug: String,
    /// The record to change (a revision, or the contribution a comment is for); None: a new one.
    record: Option<Record>,
    number: i64,
    version: i64,
    repo: String,
    /// Where the commits go on top of: theirs (`repo`, `base`) or the fork's PR branch.
    from: String,
    branch_from: String,
}

/// Creates a draft, or a new version of one, and returns its record. The owner hears about it.
pub async fn submit(
    db: &crate::db::Db,
    env: &Env<'_>,
    actor: &crate::auth::Actor,
    d: DraftIn,
) -> anyhow::Result<Record> {
    let kind = DraftType::parse(&d.kind)?;
    let repo = d.repo.trim().to_owned();
    check_text(&d.body, kind != DraftType::Commits)?;
    if kind.is_new() {
        if !d.revise.trim().is_empty() && !d.on.trim().is_empty() {
            bail!("a new PR or issue isn't `on` another one");
        }
        if d.title.trim().is_empty() || d.title.chars().count() > 256 {
            bail!("give it a title (up to 256 characters)");
        }
    }
    if kind == DraftType::Pr {
        check_branch(d.base.trim()).context("the base (their branch)")?;
        check_branch(d.branch.trim()).context("the branch (on the owner's fork)")?;
    }
    if d.why.chars().count() > 2000 {
        bail!("`why` is a sentence or two for the owner (up to 2000 characters)");
    }
    let bundle = if kind.has_code() {
        use base64::Engine as _;
        let b = base64::engine::general_purpose::STANDARD.decode(d.bundle.trim()).context("the bundle isn't base64")?;
        if b.is_empty() {
            bail!("{} needs the commits as a bundle", kind.noun());
        }
        if b.len() > MAX_BUNDLE {
            bail!("the bundle is bigger than {} MB", MAX_BUNDLE / 1024 / 1024);
        }
        b
    } else {
        if !d.bundle.is_empty() {
            bail!("{} has no commits", kind.noun());
        }
        Vec::new()
    };

    // Which record, and which version.
    let a = actor.clone();
    let dd = d.clone();
    let repo2 = repo.clone();
    let plan = db
        .call(move |c| {
            let project = crate::mcp::target_project(c, &a, dd.project.as_deref(), true)?;
            let find = |r: &str| -> anyhow::Result<Record> {
                records::try_find(c, &RecordRef::parse(r.trim())?, Some(&project.slug))?
                    .filter(|r| r.project_id == project.id && r.kind == Kind::Contribution)
                    .ok_or_else(|| anyhow::anyhow!("{} has no contribution {}", project.slug, r.trim()))
            };
            let plan = if kind.is_new() {
                crate::github::check_repo(&repo2)?;
                match dd.revise.trim() {
                    "" => {
                        crate::contributions::check_room(c, project.id)?;
                        Plan {
                            project_id: project.id,
                            slug: project.slug.clone(),
                            record: None,
                            number: 0,
                            version: 1,
                            repo: repo2.clone(),
                            from: repo2.clone(),
                            branch_from: dd.base.trim().to_owned(),
                        }
                    }
                    r => {
                        let r = find(r)?;
                        if !matches!(r.state.as_str(), "draft" | "shown") {
                            bail!("{} was {} already; a new version goes into a new draft", r.short_id(), r.state);
                        }
                        if r.str("type") != kind.as_str() || r.str("repo") != repo2 {
                            bail!("{} is a {} for {}; this isn't", r.short_id(), r.str("type"), r.str("repo"));
                        }
                        Plan {
                            project_id: project.id,
                            slug: project.slug.clone(),
                            number: r.number,
                            version: r.data.get("version").and_then(Value::as_i64).unwrap_or(1) + 1,
                            repo: repo2.clone(),
                            from: repo2.clone(),
                            branch_from: dd.base.trim().to_owned(),
                            record: Some(r),
                        }
                    }
                }
            } else {
                let on = if dd.on.trim().is_empty() { dd.revise.trim() } else { dd.on.trim() };
                if on.is_empty() {
                    bail!("say which contribution it's for, e.g. on: C-4");
                }
                let r = find(on)?;
                if !matches!(r.state.as_str(), "sent" | "merged" | "closed") || r.data.get("number").is_none() {
                    bail!("{} isn't sent yet; change its draft instead", r.short_id());
                }
                if kind == DraftType::Commits && (r.str("type") != "pr" || r.state != "sent") {
                    bail!("new commits go to an open PR; {} isn't one", r.short_id());
                }
                if kind == DraftType::Commits && r.str("fork").is_empty() {
                    bail!("{} wasn't sent through AgentFlow, so its branch isn't known here", r.short_id());
                }
                let reply = r.data.get("reply");
                let pending = reply.and_then(|v| v.get("state")).and_then(Value::as_str).unwrap_or("");
                let version = match pending {
                    "shown" | "draft" => {
                        let same = reply.and_then(|v| v.get("type")).and_then(Value::as_str) == Some(kind.as_str());
                        if !same || dd.revise.trim().is_empty() {
                            bail!(
                                "{} has a draft waiting already; send a new version of it with revise: {}",
                                r.short_id(),
                                r.short_id()
                            );
                        }
                        reply.and_then(|v| v.get("version")).and_then(Value::as_i64).unwrap_or(1) + 1
                    }
                    "sending" => bail!("the owner is sending {}'s draft right now", r.short_id()),
                    _ => 1,
                };
                Plan {
                    project_id: project.id,
                    slug: project.slug.clone(),
                    number: r.number,
                    version,
                    repo: r.str("repo").to_owned(),
                    from: r.str("fork").to_owned(),
                    branch_from: r.str("branch").to_owned(),
                    record: Some(r),
                }
            };
            if !dd.agreed.trim().is_empty() {
                let ok = records::try_find(c, &RecordRef::parse(dd.agreed.trim())?, Some(&project.slug))?
                    .is_some_and(|r| r.kind == Kind::Decision && r.project_id == project.id);
                if !ok {
                    bail!("{} isn't a decision of {}", dd.agreed.trim(), project.slug);
                }
            }
            if !dd.task.trim().is_empty() {
                records::try_find(c, &RecordRef::parse(dd.task.trim())?, Some(&project.slug))?
                    .filter(|r| r.project_id == project.id)
                    .ok_or_else(|| anyhow::anyhow!("{} has no record {}", project.slug, dd.task.trim()))?;
            }
            Ok(plan)
        })
        .await?;

    // The commits, read by AgentFlow itself. A new draft's number is only known once it exists,
    // so its git ref uses a fresh name; a revision's, its record's.
    let code = if kind.has_code() {
        let tag = if plan.number > 0 { format!("C-{}", plan.number) } else { crate::util::new_secret("n") };
        let suffix = if kind == DraftType::Commits { "r" } else { "v" };
        let git_ref = format!("refs/drafts/{}/{tag}/{suffix}{}", plan.slug, plan.version);
        let diff_file =
            env.data_dir.join("outgoing").join(&plan.slug).join(format!("{tag}-{suffix}{}.diff", plan.version));
        Some(
            import(
                env.data_dir,
                env.git_host,
                &plan.repo,
                &plan.from,
                &plan.branch_from,
                &bundle,
                &git_ref,
                &diff_file,
                kind == DraftType::Commits,
            )
            .await?,
        )
    } else {
        None
    };
    let (template, contributing) = match (&code, kind) {
        (Some(code), DraftType::Pr) => (
            their_file(env.data_dir, &plan.repo, &code.base_ref, PR_TEMPLATES).await,
            their_file(env.data_dir, &plan.repo, &code.base_ref, CONTRIBUTING).await,
        ),
        _ => (None, None),
    };

    let author = actor.name();
    db.call(move |c| {
        let mut data = Map::new();
        if let Some(code) = &code {
            data.insert("code".into(), serde_json::to_value(code)?);
        }
        data.insert("version".into(), plan.version.into());
        data.insert("why".into(), d.why.trim().into());
        data.insert("task".into(), d.task.trim().into());
        data.insert("agreed".into(), d.agreed.trim().into());
        data.insert("note".into(), Value::Null);
        data.insert("drafted_by".into(), author.clone().into());
        data.insert("drafted_at".into(), crate::util::now_ms().into());
        let (r, what) = if kind.is_new() {
            data.insert("type".into(), kind.as_str().into());
            data.insert("repo".into(), plan.repo.clone().into());
            if kind == DraftType::Pr {
                data.insert("base".into(), d.base.trim().into());
                data.insert("branch".into(), d.branch.trim().into());
                data.insert(
                    "template".into(),
                    template.map(|(n, t)| json!({"name": n, "text": t})).unwrap_or(Value::Null),
                );
                data.insert(
                    "contributing".into(),
                    contributing.map(|(n, t)| json!({"name": n, "text": t})).unwrap_or(Value::Null),
                );
            }
            match &plan.record {
                None => {
                    let r = records::create(
                        c,
                        plan.project_id,
                        &author,
                        NewRecord {
                            kind: Some(Kind::Contribution),
                            title: d.title.trim().to_owned(),
                            body: d.body.clone(),
                            state: Some("shown".into()),
                            data,
                            ..Default::default()
                        },
                    )?;
                    (r, String::new())
                }
                Some(old) => {
                    let patch = Patch {
                        title: Some(d.title.trim().to_owned()),
                        body: Some(d.body.clone()),
                        state: Some("shown".into()),
                        data,
                        quiet: true,
                        ..Default::default()
                    };
                    let (r, _) = records::update(c, old, &author, patch)?;
                    let what = format!("sent version {} of the draft: {}", plan.version, r.title);
                    (r, what)
                }
            }
        } else {
            let old = plan.record.as_ref().expect("a comment or commits are for a record");
            let mut reply = Map::new();
            reply.insert("type".into(), kind.as_str().into());
            reply.insert("body".into(), d.body.clone().into());
            reply.insert("state".into(), "shown".into());
            for (k, v) in data {
                reply.insert(k, v);
            }
            let mut patch_data = Map::new();
            patch_data.insert("reply".into(), Value::Object(reply));
            let (r, _) =
                records::update(c, old, &author, Patch { data: patch_data, quiet: true, ..Default::default() })?;
            let what = if plan.version > 1 {
                format!("sent version {} of the {} draft for {}", plan.version, kind.as_str(), r.title)
            } else {
                format!(
                    "drafted {} for {}",
                    if kind == DraftType::Comment { "a comment" } else { "new commits" },
                    r.title
                )
            };
            (r, what)
        };
        // A new record has its own "created" event already.
        if plan.record.is_some() {
            records::event(c, Some(r.project_id), Some(r.id), "contribution.drafted", &what, &author)?;
        }
        Ok(r)
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// The owner's side: what the draft page shows, and its three buttons

/// The draft waiting for the owner on a contribution, if any: a new PR or issue itself, or the
/// comment or commits for one already sent.
pub struct Pending<'a> {
    pub kind: DraftType,
    /// Where the draft's fields are: the record's data, or its reply.
    pub data: &'a Map<String, Value>,
    /// shown (waiting for the owner), draft (back with the agent), sending
    pub state: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub version: i64,
}

pub fn pending(r: &Record) -> Option<Pending<'_>> {
    if r.kind != Kind::Contribution {
        return None;
    }
    let obj = r.data.as_object()?;
    if matches!(r.state.as_str(), "draft" | "shown" | "approved") {
        let kind = DraftType::parse(r.str("type")).ok()?;
        return Some(Pending {
            kind,
            data: obj,
            state: if r.state == "approved" { "sending" } else { r.state.as_str() },
            title: &r.title,
            body: &r.body,
            version: obj.get("version").and_then(Value::as_i64).unwrap_or(1),
        });
    }
    let reply = obj.get("reply")?.as_object()?;
    let state = reply.get("state").and_then(Value::as_str)?;
    if !matches!(state, "shown" | "draft" | "sending") {
        return None;
    }
    Some(Pending {
        kind: DraftType::parse(reply.get("type").and_then(Value::as_str)?).ok()?,
        data: reply,
        state,
        title: &r.title,
        body: reply.get("body").and_then(Value::as_str).unwrap_or(""),
        version: reply.get("version").and_then(Value::as_i64).unwrap_or(1),
    })
}

/// "New PR to ophymx/formulary: Center fences", "Comment on ophymx/formulary#4: …".
pub fn heading(r: &Record, p: &Pending<'_>) -> String {
    match p.kind {
        DraftType::Pr => format!("New PR to {}: {}", r.str("repo"), p.title),
        DraftType::Issue => format!("New issue for {}: {}", r.str("repo"), p.title),
        DraftType::Comment => format!("Comment on {}", r.title),
        DraftType::Commits => format!("New commits for {}", r.title),
    }
}

/// Waiting for the owner's Send?
pub fn waits_for_owner(r: &Record) -> bool {
    pending(r).is_some_and(|p| p.state == "shown")
}

/// What exactly gets pushed and where, as a fingerprint: the page sends it with the owner's press,
/// and Send refuses if the draft changed since the page was opened.
pub fn fingerprint(r: &Record) -> String {
    let Some(p) = pending(r) else { return String::new() };
    let code = p.data.get("code").cloned().unwrap_or(Value::Null);
    let parts = json!([
        r.id,
        p.kind.as_str(),
        p.version,
        r.str("repo"),
        r.str("base"),
        r.str("branch"),
        r.data.get("number"),
        code.get("sha"),
        code.get("base_sha"),
    ]);
    crate::util::hex(&Sha256::digest(parts.to_string().as_bytes())[..12])
}

/// One line of the checks under the draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub ok: bool,
    pub text: String,
    pub link: Option<(String, String)>,
}

/// Words that mark Claude's attribution, which the owner leaves off other people's repositories.
const ATTRIBUTION: &[&str] = &[
    "co-authored-by: claude",
    "claude-session:",
    "generated with [claude code",
    "generated with claude code",
    "noreply@anthropic.com",
];

pub fn has_attribution(text: &str) -> bool {
    let t = text.to_lowercase();
    ATTRIBUTION.iter().any(|w| t.contains(w))
}

/// Headings of their template that the text doesn't have.
pub fn missing_headings(template: &str, body: &str) -> Vec<String> {
    let body = body.to_lowercase();
    template
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim().to_owned())
        .filter(|h| !h.is_empty() && !body.contains(&h.to_lowercase()))
        .collect()
}

pub fn checks(conn: &Connection, r: &Record, p: &Pending<'_>) -> anyhow::Result<Vec<Check>> {
    let mut out = Vec::new();
    let commits: Vec<Commit> = p
        .data
        .get("code")
        .and_then(|c| c.get("commits"))
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default();
    if let Some(t) = r.data.get("template").and_then(|t| t.get("text")).and_then(Value::as_str)
        && p.kind == DraftType::Pr
    {
        let name = r.data.get("template").and_then(|t| t.get("name")).and_then(Value::as_str).unwrap_or("");
        let missing = missing_headings(t, p.body);
        out.push(Check {
            ok: missing.is_empty(),
            text: if missing.is_empty() {
                format!("Has every heading of their PR template ({name})")
            } else {
                format!("Their PR template ({name}) has headings the text doesn't: {}", missing.join(", "))
            },
            link: None,
        });
    }
    let in_commits = commits.iter().any(|c| has_attribution(&format!("{}\n{}", c.subject, c.body)));
    let in_text = has_attribution(p.body) || (p.kind.is_new() && has_attribution(p.title));
    out.push(Check {
        ok: !in_commits && !in_text,
        text: match (in_text, in_commits) {
            (false, false) => "No Claude attribution in the text or the commits".into(),
            (true, false) => "The text has Claude attribution (you leave it off other people's repositories)".into(),
            (false, true) => {
                "A commit message has Claude attribution (you leave it off other people's repositories)".into()
            }
            (true, true) => "The text and a commit message have Claude attribution".into(),
        },
        link: None,
    });
    let agreed = p.data.get("agreed").and_then(Value::as_str).unwrap_or("");
    if p.kind.is_new() {
        let found = if agreed.is_empty() {
            None
        } else {
            records::try_find(conn, &RecordRef::parse(agreed)?, Some(&r.project))?.filter(|d| d.kind == Kind::Decision)
        };
        out.push(match found {
            Some(d) => Check { ok: true, text: "You agreed to it:".into(), link: Some((d.ref_id.clone(), d.url())) },
            None => Check { ok: false, text: "No decision of yours is linked to it".into(), link: None },
        });
    }
    Ok(out)
}

/// The owner asks for changes: their edits are kept, their note goes to the agent word for word,
/// and the draft waits for a new version.
pub fn ask_changes(conn: &Connection, r: &Record, title: &str, body: &str, note: &str) -> anyhow::Result<Record> {
    let note = note.trim();
    if note.is_empty() {
        bail!("say what should change");
    }
    let p = pending(r).context("there's no draft here")?;
    if p.state != "shown" {
        bail!("the draft isn't waiting for you");
    }
    let r = save_edits(conn, r, title, body)?;
    let r = set_pending_state(conn, &r, "draft", Some(note))?;
    let what = format!("The owner asked for changes to the draft {}: \"{note}\"", r.short_id());
    records::event(conn, Some(r.project_id), Some(r.id), "contribution.changes", &what, "owner")?;
    Ok(r)
}

/// The owner drops the draft: nothing is sent; the agent hears it, with the owner's reason if any.
pub fn drop_draft(conn: &Connection, r: &Record, note: &str) -> anyhow::Result<Record> {
    let p = pending(r).context("there's no draft here")?;
    if !matches!(p.state, "shown" | "draft") {
        bail!("the draft isn't waiting for you");
    }
    let new_record = p.kind.is_new();
    let note = note.trim();
    let r = if new_record {
        let (r, _) = records::update(
            conn,
            r,
            "owner",
            Patch { state: Some("dropped".into()), quiet: true, ..Default::default() },
        )?;
        r
    } else {
        set_pending_state(conn, r, "dropped", (!note.is_empty()).then_some(note))?
    };
    let what = if note.is_empty() {
        format!("The owner dropped the draft {}; nothing was sent.", r.short_id())
    } else {
        format!("The owner dropped the draft {}; nothing was sent: \"{note}\"", r.short_id())
    };
    records::event(conn, Some(r.project_id), Some(r.id), "contribution.dropped", &what, "owner")?;
    Ok(r)
}

/// Keeps the owner's edits of the title and text.
fn save_edits(conn: &Connection, r: &Record, title: &str, body: &str) -> anyhow::Result<Record> {
    let p = pending(r).context("there's no draft here")?;
    check_text(body, p.kind != DraftType::Commits)?;
    if p.kind.is_new() {
        if title.trim().is_empty() || title.chars().count() > 256 {
            bail!("the title can't be empty (up to 256 characters)");
        }
        let (r, _) = records::update(
            conn,
            r,
            "owner",
            Patch { title: Some(title.trim().into()), body: Some(body.into()), quiet: true, ..Default::default() },
        )?;
        Ok(r)
    } else {
        let mut reply = p.data.clone();
        if reply.get("body").and_then(Value::as_str) == Some(body) {
            return Ok(r.clone());
        }
        reply.insert("body".into(), body.into());
        let mut data = Map::new();
        data.insert("reply".into(), Value::Object(reply));
        let (r, _) = records::update(conn, r, "owner", Patch { data, quiet: true, ..Default::default() })?;
        Ok(r)
    }
}

fn set_pending_state(conn: &Connection, r: &Record, state: &str, note: Option<&str>) -> anyhow::Result<Record> {
    let p = pending(r).context("there's no draft here")?;
    if p.kind.is_new() {
        let mut data = Map::new();
        if let Some(n) = note {
            data.insert("note".into(), n.into());
        }
        let record_state = if state == "sending" { "approved" } else { state };
        let (r, _) = records::update(
            conn,
            r,
            "owner",
            Patch { state: Some(record_state.into()), data, quiet: true, ..Default::default() },
        )?;
        Ok(r)
    } else {
        let mut reply = p.data.clone();
        reply.insert("state".into(), state.into());
        if let Some(n) = note {
            reply.insert("note".into(), n.into());
        }
        let mut data = Map::new();
        data.insert("reply".into(), Value::Object(reply));
        let (r, _) = records::update(conn, r, "owner", Patch { data, quiet: true, ..Default::default() })?;
        Ok(r)
    }
}

// ---------------------------------------------------------------------------------------------
// Sending, with the owner's token for other people's repositories

/// What GitHub needs to do for a Send. The real one runs `gh` with the external token; tests use a
/// fake.
pub trait Poster: Send + Sync {
    /// The external token, read when it's needed; None: sending isn't set up.
    fn token(&self) -> anyhow::Result<String>;
    /// The token's account, e.g. "giosuel".
    fn login<'a>(&'a self, token: &'a str) -> futures::future::BoxFuture<'a, anyhow::Result<String>>;
    /// Makes sure the account has a fork of `repo`; returns the fork, e.g. "giosuel/formulary".
    fn fork<'a>(&'a self, token: &'a str, repo: &'a str) -> futures::future::BoxFuture<'a, anyhow::Result<String>>;
    /// Opens a PR or an issue, or posts a comment; returns its URL.
    fn post<'a>(&'a self, token: &'a str, post: &'a Post) -> futures::future::BoxFuture<'a, anyhow::Result<String>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Post {
    Pr { repo: String, base: String, head: String, title: String, body: String },
    Issue { repo: String, title: String, body: String },
    Comment { repo: String, number: i64, body: String },
}

/// The real one: `gh` with the token from `external_token_file` in its environment.
pub struct GhPoster {
    pub token_file: String,
}

async fn gh(token: &str, args: &[&str], body: Option<&str>) -> anyhow::Result<String> {
    // The text goes in through a file, not the command line.
    let file = match body {
        Some(b) => {
            let f = tempfile::NamedTempFile::new()?;
            std::fs::write(f.path(), b)?;
            Some(f)
        }
        None => None,
    };
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args(args);
    if let Some(f) = &file {
        cmd.arg("--body-file").arg(f.path());
    }
    cmd.env("GH_TOKEN", token).env("GH_PROMPT_DISABLED", "1").env("NO_COLOR", "1").kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(120), cmd.output())
        .await
        .context("gh took longer than 2 minutes")?
        .context("running gh")?;
    let blank = |s: &[u8]| String::from_utf8_lossy(s).replace(token, "…");
    if !out.status.success() {
        bail!("gh {}: {}", args.first().unwrap_or(&""), blank(&out.stderr).trim().lines().last().unwrap_or("failed"));
    }
    Ok(blank(&out.stdout).trim().to_owned())
}

impl Poster for GhPoster {
    fn token(&self) -> anyhow::Result<String> {
        if self.token_file.trim().is_empty() {
            bail!(
                "sending isn't set up: external_token_file in AgentFlow's config names the token for other people's repositories"
            );
        }
        let path = crate::util::expand_home(self.token_file.trim());
        let t = std::fs::read_to_string(&path)
            .with_context(|| format!("reading the external token ({})", path.display()))?;
        let t = t.trim().to_owned();
        if t.is_empty() || t.contains(char::is_whitespace) {
            bail!("the external token file doesn't hold a token");
        }
        Ok(t)
    }

    fn login<'a>(&'a self, token: &'a str) -> futures::future::BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move { gh(token, &["api", "user", "--jq", ".login"], None).await })
    }

    fn fork<'a>(&'a self, token: &'a str, repo: &'a str) -> futures::future::BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            let login = self.login(token).await?;
            let name = repo.split('/').nth(1).unwrap_or(repo);
            let mine = format!("{login}/{name}");
            if gh(token, &["api", &format!("repos/{mine}"), "--jq", ".full_name"], None).await.is_err() {
                gh(token, &["repo", "fork", repo, "--clone=false", "--default-branch-only"], None).await?;
                // GitHub makes the fork in the background.
                for _ in 0..20 {
                    if gh(token, &["api", &format!("repos/{mine}"), "--jq", ".full_name"], None).await.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
            Ok(mine)
        })
    }

    fn post<'a>(&'a self, token: &'a str, post: &'a Post) -> futures::future::BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            let out = match post {
                Post::Pr { repo, base, head, title, body } => {
                    gh(
                        token,
                        &["pr", "create", "-R", repo, "--base", base, "--head", head, "--title", title],
                        Some(body),
                    )
                    .await?
                }
                Post::Issue { repo, title, body } => {
                    gh(token, &["issue", "create", "-R", repo, "--title", title], Some(body)).await?
                }
                Post::Comment { repo, number, body } => {
                    gh(token, &["issue", "comment", &number.to_string(), "-R", repo], Some(body)).await?
                }
            };
            out.lines()
                .rev()
                .find(|l| l.starts_with("https://github.com/"))
                .map(str::to_owned)
                .with_context(|| format!("gh didn't say where it posted: {out}"))
        })
    }
}

/// The number at the end of a PR's, issue's or comment's URL
/// (…/pull/12, …/issues/12, …/issues/12#issuecomment-…).
fn number_of(url: &str) -> Option<i64> {
    let path = url.split('#').next()?;
    path.rsplit('/').next()?.parse().ok()
}

/// The owner pressed Send: saves their text, pushes the commits (if any) to their fork, posts it as
/// them, and tells the agent. `fingerprint` is the one the page showed; a draft that changed since
/// is refused. Returns where it was posted.
pub async fn send(
    db: &crate::db::Db,
    env: &Env<'_>,
    poster: &dyn Poster,
    record_id: i64,
    title: String,
    body: String,
    fingerprint_seen: String,
) -> anyhow::Result<String> {
    // Claim it, so a second press (or tab) can't send it twice.
    let r = db
        .call(move |c| {
            let r = records::get(c, record_id)?;
            if fingerprint(&r) != fingerprint_seen {
                bail!("the draft changed since you opened it; look at it again");
            }
            let p = pending(&r).context("there's no draft here")?;
            if p.state != "shown" {
                bail!("the draft isn't waiting for you (it's {})", p.state);
            }
            let r = save_edits(c, &r, &title, &body)?;
            set_pending_state(c, &r, "sending", None)
        })
        .await?;
    let result = push_and_post(env, poster, &r).await;
    let id = r.id;
    match result {
        Ok((url, fork)) => {
            let url2 = url.clone();
            db.call(move |c| {
                let r = records::get(c, id)?;
                finish_sent(c, &r, &url2, fork.as_deref())
            })
            .await?;
            Ok(url)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            db.call(move |c| {
                let r = records::get(c, id)?;
                set_pending_state(c, &r, "shown", None)?;
                records::event(
                    c,
                    Some(r.project_id),
                    Some(r.id),
                    "contribution.failed",
                    &format!("Sending {} failed: {msg}", r.short_id()),
                    "github",
                )?;
                Ok(())
            })
            .await?;
            Err(e)
        }
    }
}

/// Returns where it was posted, and (for a new PR) the fork.
async fn push_and_post(env: &Env<'_>, poster: &dyn Poster, r: &Record) -> anyhow::Result<(String, Option<String>)> {
    let p = pending(r).context("there's no draft here")?;
    let token = poster.token()?;
    let repo = r.str("repo").to_owned();
    let code: Option<Code> = p.data.get("code").and_then(|c| serde_json::from_value(c.clone()).ok());
    // The commits go to the owner's fork first.
    let fork = if p.kind.has_code() {
        let code = code.as_ref().context("the draft has no commits")?;
        let fork = if p.kind == DraftType::Pr { poster.fork(&token, &repo).await? } else { r.str("fork").to_owned() };
        let branch = r.str("branch").to_owned();
        check_branch(&branch)?;
        let dir = cache_repo(env.data_dir, &repo).await?;
        let url = format!("{}/{fork}.git", env.git_host.trim_end_matches('/'));
        // The token reaches git through the environment and a helper, never the command line.
        let helper = "!f() { echo username=x-access-token; echo \"password=$AF_PUSH_TOKEN\"; }; f";
        git(
            &dir,
            &[
                "-c",
                "credential.helper=",
                "-c",
                &format!("credential.helper={helper}"),
                "push",
                "-q",
                &url,
                &format!("{}:refs/heads/{branch}", code.sha),
            ],
            Some(&token),
        )
        .await
        .with_context(|| format!("pushing to {fork}"))?;
        Some(fork)
    } else {
        None
    };
    let post = match p.kind {
        DraftType::Pr => Some(Post::Pr {
            repo: repo.clone(),
            base: r.str("base").to_owned(),
            head: format!("{}:{}", fork.as_deref().unwrap_or("").split('/').next().unwrap_or(""), r.str("branch")),
            title: p.title.to_owned(),
            body: p.body.to_owned(),
        }),
        DraftType::Issue => {
            Some(Post::Issue { repo: repo.clone(), title: p.title.to_owned(), body: p.body.to_owned() })
        }
        DraftType::Comment => Some(Post::Comment {
            repo: repo.clone(),
            number: r.data.get("number").and_then(Value::as_i64).context("no number")?,
            body: p.body.to_owned(),
        }),
        // New commits are on the PR once pushed; a note with them is a comment.
        DraftType::Commits => (!p.body.trim().is_empty()).then(|| Post::Comment {
            repo: repo.clone(),
            number: r.data.get("number").and_then(Value::as_i64).unwrap_or(0),
            body: p.body.to_owned(),
        }),
    };
    let url = match post {
        Some(post) => poster.post(&token, &post).await?,
        None => r.str("url").to_owned(),
    };
    Ok((url, fork))
}

fn finish_sent(conn: &Connection, r: &Record, url: &str, fork: Option<&str>) -> anyhow::Result<Record> {
    let p = pending(r).context("there's no draft here")?;
    let kind = p.kind;
    let now = crate::util::now_ms();
    let (r, what) = if kind.is_new() {
        let number = number_of(url).context("no number in the URL")?;
        let mut data = Map::new();
        data.insert("number".into(), number.into());
        data.insert("url".into(), url.into());
        data.insert("sent_title".into(), r.title.clone().into());
        data.insert("sent_body".into(), r.body.clone().into());
        data.insert("sent_at".into(), now.into());
        data.insert("sent_by".into(), "owner".into());
        // The watcher starts from nothing new.
        for k in ["comments", "reviews", "review_comments"] {
            data.insert(k.into(), 0.into());
        }
        if let Some(f) = fork {
            data.insert("fork".into(), f.into());
        }
        let title = format!("{}#{number}: {}", r.str("repo"), r.title);
        let (r, _) = records::update(
            conn,
            r,
            "owner",
            Patch { title: Some(title), state: Some("sent".into()), data, quiet: true, ..Default::default() },
        )?;
        let what = format!(
            "The owner sent {} {}: {url}",
            if kind == DraftType::Pr { "the PR" } else { "the issue" },
            r.short_id()
        );
        (r, what)
    } else {
        let mut reply = p.data.clone();
        reply.insert("state".into(), "sent".into());
        reply.insert("url".into(), url.into());
        reply.insert("sent_at".into(), now.into());
        let mut sent = r.data.get("replies_sent").and_then(Value::as_array).cloned().unwrap_or_default();
        sent.push(Value::Object(reply.clone()));
        let mut data = Map::new();
        data.insert("reply".into(), Value::Object(reply));
        data.insert("replies_sent".into(), Value::Array(sent));
        let (r, _) = records::update(conn, r, "owner", Patch { data, quiet: true, ..Default::default() })?;
        let what = match kind {
            DraftType::Comment => format!("The owner posted the comment on {}: {url}", r.title),
            _ => format!("The owner pushed the new commits to {}: {}", r.title, r.str("url")),
        };
        (r, what)
    };
    records::event(conn, Some(r.project_id), Some(r.id), "contribution.sent", &what, "owner")?;
    Ok(r)
}

/// The draft's diff, as read when it arrived.
pub fn diff_text(p: &Pending<'_>) -> Option<String> {
    let file = p.data.get("code")?.get("diff_file")?.as_str()?;
    std::fs::read_to_string(file).ok()
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::Mutex;

    /// Posts nothing; remembers what it was asked to post.
    #[derive(Default)]
    pub struct FakePoster {
        pub posted: Mutex<Vec<Post>>,
        pub no_token: bool,
    }

    impl Poster for FakePoster {
        fn token(&self) -> anyhow::Result<String> {
            if self.no_token { bail!("sending isn't set up") } else { Ok("ghp_secret".into()) }
        }
        fn login<'a>(&'a self, _: &'a str) -> futures::future::BoxFuture<'a, anyhow::Result<String>> {
            Box::pin(async { Ok("giosuel".to_owned()) })
        }
        fn fork<'a>(&'a self, _: &'a str, repo: &'a str) -> futures::future::BoxFuture<'a, anyhow::Result<String>> {
            Box::pin(async move { Ok(format!("giosuel/{}", repo.split('/').nth(1).unwrap_or(""))) })
        }
        fn post<'a>(&'a self, _: &'a str, post: &'a Post) -> futures::future::BoxFuture<'a, anyhow::Result<String>> {
            let mut list = self.posted.lock().unwrap();
            list.push(post.clone());
            let n = list.len() as i64 + 40;
            let url = match post {
                Post::Pr { repo, .. } => format!("https://github.com/{repo}/pull/{n}"),
                Post::Issue { repo, .. } => format!("https://github.com/{repo}/issues/{n}"),
                Post::Comment { repo, number, .. } => {
                    format!("https://github.com/{repo}/issues/{number}#issuecomment-{n}")
                }
            };
            Box::pin(async move { Ok(url) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_are_checked() {
        for ok in ["main", "center-fences", "fix/fences.v2", "a_b"] {
            assert!(check_branch(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-x", "/x", "a..b", "a b", "x/", "x.lock", "a//b", ".x", "x\n", "refs/heads/../x"] {
            assert!(check_branch(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn attribution_and_template_headings() {
        assert!(has_attribution("Fix\n\nCo-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"));
        assert!(has_attribution("🤖 Generated with [Claude Code](https://claude.com/claude-code)"));
        assert!(!has_attribution("Center fences on the axis"));
        let t = "## Summary\n\nWhat it does\n\n## Testing\n\n<!-- how -->\n";
        assert_eq!(missing_headings(t, "## Summary\nIt centers them."), ["Testing"]);
        assert!(missing_headings(t, "## summary\nx\n## testing\ny").is_empty());
    }

    #[test]
    fn numbers_from_urls() {
        assert_eq!(number_of("https://github.com/o/r/pull/12"), Some(12));
        assert_eq!(number_of("https://github.com/o/r/issues/7#issuecomment-99"), Some(7));
        assert_eq!(number_of("https://github.com/o/r"), None);
    }
}
