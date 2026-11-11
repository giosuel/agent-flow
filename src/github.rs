//! What AgentFlow asks GitHub, through the `gh` CLI (already signed in with the owner's limited
//! token on this machine). Behind a trait so tests can use a fake.

use std::future::Future;
use std::pin::Pin;

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrInfo {
    pub number: i64,
    pub title: String,
    pub url: String,
    /// open, merged or closed
    pub state: String,
    pub draft: bool,
    pub base: String,
    pub head: String,
    /// mergeable, conflicting or unknown
    pub mergeable: String,
    /// passing, failing, pending or none
    pub ci: String,
}

/// A PR or issue on someone else's repository, as the watcher sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemInfo {
    pub title: String,
    pub url: String,
    pub is_pr: bool,
    /// open, closed or merged
    pub state: String,
    pub comments: i64,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait GitHub: Send + Sync {
    fn pr<'a>(&'a self, repo: &'a str, number: i64) -> BoxFuture<'a, anyhow::Result<PrInfo>>;
    fn open_prs<'a>(&'a self, repo: &'a str) -> BoxFuture<'a, anyhow::Result<Vec<PrInfo>>>;
    fn item<'a>(&'a self, repo: &'a str, number: i64) -> BoxFuture<'a, anyhow::Result<ItemInfo>>;
}

const FIELDS: &str = "number,title,url,state,isDraft,baseRefName,headRefName,mergeable,statusCheckRollup";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPr {
    number: i64,
    title: String,
    url: String,
    state: String,
    #[serde(default)]
    is_draft: bool,
    base_ref_name: String,
    head_ref_name: String,
    #[serde(default)]
    mergeable: String,
    #[serde(default)]
    status_check_rollup: Vec<serde_json::Value>,
}

impl From<GhPr> for PrInfo {
    fn from(p: GhPr) -> Self {
        PrInfo {
            number: p.number,
            title: p.title,
            url: p.url,
            state: p.state.to_lowercase(),
            draft: p.is_draft,
            base: p.base_ref_name,
            head: p.head_ref_name,
            mergeable: p.mergeable.to_lowercase(),
            ci: ci_state(&p.status_check_rollup),
        }
    }
}

/// Folds GitHub's checks into one word. Check runs have a conclusion; commit statuses a state.
fn ci_state(checks: &[serde_json::Value]) -> String {
    if checks.is_empty() {
        return "none".into();
    }
    let mut pending = false;
    for c in checks {
        let s = |k: &str| c.get(k).and_then(|v| v.as_str()).unwrap_or("").to_uppercase();
        let verdict = if c.get("conclusion").is_some() || c.get("status").is_some() {
            if s("status") != "COMPLETED" && !s("status").is_empty() { "PENDING".to_owned() } else { s("conclusion") }
        } else {
            s("state")
        };
        match verdict.as_str() {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            "PENDING" | "EXPECTED" | "QUEUED" | "IN_PROGRESS" | "" => pending = true,
            _ => return "failing".into(),
        }
    }
    if pending { "pending".into() } else { "passing".into() }
}

/// The real thing: runs `gh`.
pub struct GhCli;

impl GhCli {
    async fn run(args: &[&str]) -> anyhow::Result<Vec<u8>> {
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            tokio::process::Command::new("gh").args(args).kill_on_drop(true).output(),
        )
        .await
        .context("gh took longer than a minute")?
        .context("running gh")?;
        if !out.status.success() {
            bail!("gh {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out.stdout)
    }
}

pub fn check_repo(repo: &str) -> anyhow::Result<()> {
    let ok = repo.split('/').count() == 2
        && repo.split('/').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)));
    if !ok {
        bail!("{repo:?} isn't a GitHub repository like owner/name");
    }
    Ok(())
}

#[derive(Deserialize)]
struct GhIssue {
    title: String,
    html_url: String,
    state: String,
    comments: i64,
    pull_request: Option<serde_json::Value>,
}

impl GitHub for GhCli {
    fn item<'a>(&'a self, repo: &'a str, number: i64) -> BoxFuture<'a, anyhow::Result<ItemInfo>> {
        Box::pin(async move {
            check_repo(repo)?;
            // The issues endpoint answers for PRs too; only a PR's own endpoint knows "merged".
            let out = Self::run(&["api", &format!("repos/{repo}/issues/{number}")]).await?;
            let i: GhIssue = serde_json::from_slice(&out).context("reading gh's answer")?;
            let is_pr = i.pull_request.is_some();
            let mut state = i.state.to_lowercase();
            if is_pr && state == "closed" {
                let out = Self::run(&["api", &format!("repos/{repo}/pulls/{number}"), "--jq", ".merged"]).await?;
                if String::from_utf8_lossy(&out).trim() == "true" {
                    state = "merged".into();
                }
            }
            Ok(ItemInfo { title: i.title, url: i.html_url, is_pr, state, comments: i.comments })
        })
    }

    fn pr<'a>(&'a self, repo: &'a str, number: i64) -> BoxFuture<'a, anyhow::Result<PrInfo>> {
        Box::pin(async move {
            check_repo(repo)?;
            let n = number.to_string();
            let out = Self::run(&["pr", "view", &n, "-R", repo, "--json", FIELDS]).await?;
            Ok(serde_json::from_slice::<GhPr>(&out).context("reading gh's answer")?.into())
        })
    }

    fn open_prs<'a>(&'a self, repo: &'a str) -> BoxFuture<'a, anyhow::Result<Vec<PrInfo>>> {
        Box::pin(async move {
            check_repo(repo)?;
            let out =
                Self::run(&["pr", "list", "-R", repo, "--state", "open", "--limit", "100", "--json", FIELDS]).await?;
            let prs: Vec<GhPr> = serde_json::from_slice(&out).context("reading gh's answer")?;
            Ok(prs.into_iter().map(Into::into).collect())
        })
    }
}

#[cfg(test)]
pub mod fake {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    /// A GitHub that answers from a map tests can change.
    #[derive(Default)]
    pub struct FakeGitHub {
        pub items: Mutex<HashMap<(String, i64), ItemInfo>>,
        pub prs: Mutex<HashMap<(String, i64), PrInfo>>,
        /// PRs reported merged the next time they're fetched one by one (as after a merge button).
        pub merge_on_fetch: Mutex<std::collections::HashSet<i64>>,
    }

    impl FakeGitHub {
        pub fn set(&self, repo: &str, pr: PrInfo) {
            self.prs.lock().unwrap().insert((repo.into(), pr.number), pr);
        }
    }

    impl GitHub for FakeGitHub {
        fn pr<'a>(&'a self, repo: &'a str, number: i64) -> BoxFuture<'a, anyhow::Result<PrInfo>> {
            let merge = self.merge_on_fetch.lock().unwrap().contains(&number);
            let r =
                self.prs.lock().unwrap().get(&(repo.to_owned(), number)).cloned().context("no such PR").map(|mut p| {
                    if merge {
                        p.state = "merged".into();
                    }
                    p
                });
            Box::pin(async move { r })
        }

        fn item<'a>(&'a self, repo: &'a str, number: i64) -> BoxFuture<'a, anyhow::Result<ItemInfo>> {
            let r = self.items.lock().unwrap().get(&(repo.to_owned(), number)).cloned().context("no such item");
            Box::pin(async move { r })
        }

        fn open_prs<'a>(&'a self, repo: &'a str) -> BoxFuture<'a, anyhow::Result<Vec<PrInfo>>> {
            let mut v: Vec<PrInfo> = self
                .prs
                .lock()
                .unwrap()
                .iter()
                .filter(|((r, _), p)| r == repo && p.state == "open")
                .map(|(_, p)| p.clone())
                .collect();
            v.sort_by_key(|p| p.number);
            Box::pin(async move { Ok(v) })
        }
    }

    pub fn pr(number: i64, base: &str, head: &str) -> PrInfo {
        PrInfo {
            number,
            title: format!("PR {number}"),
            url: format!("https://github.com/o/r/pull/{number}"),
            state: "open".into(),
            draft: false,
            base: base.into(),
            head: head.into(),
            mergeable: "mergeable".into(),
            ci: "passing".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ci_folds_checks() {
        assert_eq!(ci_state(&[]), "none");
        let ok = json!({"status": "COMPLETED", "conclusion": "SUCCESS"});
        let running = json!({"status": "IN_PROGRESS", "conclusion": ""});
        let failed = json!({"status": "COMPLETED", "conclusion": "FAILURE"});
        let status_ok = json!({"state": "SUCCESS"});
        assert_eq!(ci_state(&[ok.clone(), status_ok]), "passing");
        assert_eq!(ci_state(&[ok.clone(), running]), "pending");
        assert_eq!(ci_state(&[ok, failed]), "failing");
    }

    #[test]
    fn repos_are_checked() {
        assert!(check_repo("giosuel/agent-flow").is_ok());
        for bad in ["giosuel", "a/b/c", "a/--x;rm", "/x", "a b/c"] {
            assert!(check_repo(bad).is_err(), "{bad}");
        }
    }
}
