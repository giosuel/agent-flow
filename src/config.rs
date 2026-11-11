//! Server configuration, read from `~/.config/agentflow/config.toml`. Every field has a default, so
//! the file is optional.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::util::expand_home;

pub const SYSTEM_PATH: &str = "/etc/agentflow/config.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where the server listens. Never a wildcard address: the machine has a public IP.
    pub listen: SocketAddr,
    /// The address people and agents use, e.g. `https://flow.giosuel.io`. Decides the cookie, the
    /// passkey relying party, the origin cookie posts must come from, and the MCP endpoint's
    /// allowed Host headers.
    pub public_url: String,
    /// Database, VAPID keys, backups and log archives.
    pub data_dir: String,
    /// Requests from these peers may set X-Forwarded-For (Caddy on the same machine).
    pub trusted_proxies: Vec<std::net::IpAddr>,
    /// Where project and consultant tokens are written for Claude Code's headersHelper.
    pub token_dir: String,
    /// The checkout of the guides repository (ai-handbook) that agents read and the owner edits.
    pub handbook_dir: String,
    /// That repository on GitHub, for its pull requests and links.
    pub handbook_repo: String,
    /// Who the owner's guide edits are committed as.
    pub git_author: String,
    /// False on a test instance: the Guides page's Merge button refuses instead of merging on GitHub.
    pub github_writes: bool,
    /// The contact push services ask senders for (Apple requires one): mailto: or https:.
    pub push_contact: String,
    /// Owner sessions end after this many days without a request…
    pub session_idle_days: i64,
    /// …and after this many days in any case.
    pub session_max_days: i64,
    /// GB a build upload must leave free on the data disk, which other projects share.
    pub build_free_gb: u64,
    /// Claude Code's transcripts, read for token usage.
    pub transcripts_dir: String,
    /// When AgentFlow runs as its own user: the agents' home folder, where `~/` in the other
    /// paths points (e.g. "/home/viber"). Empty: this user's own home.
    pub agents_home: String,
    /// When AgentFlow runs as its own user: the socket of `agentflow hands`, which does what has
    /// to happen as the agents' user. Empty: do it in the server (one user for everything).
    pub hands_socket: String,
    /// The file with the GitHub token for other people's repositories (public_repo), used only
    /// when the owner presses Send on a draft. Empty: drafts can't be sent.
    pub external_token_file: String,
    /// Where repositories are fetched from and pushed to (tests use a local folder).
    pub git_host: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8790".parse().unwrap(),
            public_url: "http://127.0.0.1:8790".into(),
            data_dir: "~/.local/state/agentflow".into(),
            token_dir: "~/.config/agentflow/tokens".into(),
            push_contact: "mailto:me@giosuel.ch".into(),
            handbook_dir: "~/ai-handbook".into(),
            handbook_repo: "giosuel/ai-handbook".into(),
            git_author: "giosuel <me@giosuel.ch>".into(),
            github_writes: true,
            trusted_proxies: vec!["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()],
            session_idle_days: 14,
            session_max_days: 30,
            build_free_gb: 10,
            transcripts_dir: "~/.claude/projects".into(),
            agents_home: String::new(),
            hands_socket: String::new(),
            external_token_file: String::new(),
            git_host: "https://github.com".into(),
        }
    }
}

impl Config {
    /// `~/.config/agentflow/config.toml` (a single-user install), else `/etc/agentflow/config.toml`
    /// (the packaged system service) when that exists.
    pub fn default_path() -> PathBuf {
        let user = expand_home("~/.config/agentflow/config.toml");
        let system = PathBuf::from(SYSTEM_PATH);
        if !user.exists() && system.exists() { system } else { user }
    }

    /// Points `~/` at the agents' home and jobs at the helper, as the config says. Once, at start.
    pub fn apply(&self) {
        if !self.agents_home.is_empty() {
            crate::util::set_agents_home(self.agents_home.clone().into());
        }
        crate::hands::init((!self.hands_socket.is_empty()).then(|| PathBuf::from(&self.hands_socket)));
    }

    /// Reads the file if it exists; a missing file means all defaults.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let config: Config = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        config.check()?;
        Ok(config)
    }

    fn check(&self) -> anyhow::Result<()> {
        if self.listen.ip().is_unspecified() {
            bail!("listen = {} would be reachable from the internet; use 127.0.0.1 or 10.0.8.1", self.listen);
        }
        if !(self.public_url.starts_with("http://") || self.public_url.starts_with("https://")) {
            bail!("public_url must start with http:// or https://");
        }
        if !self.agents_home.is_empty() && !Path::new(&self.agents_home).is_absolute() {
            bail!("agents_home must be an absolute path like /home/viber");
        }
        Ok(())
    }

    pub fn data_dir(&self) -> PathBuf {
        expand_home(&self.data_dir)
    }

    pub fn token_dir(&self) -> PathBuf {
        expand_home(&self.token_dir)
    }

    pub fn handbook_dir(&self) -> PathBuf {
        expand_home(&self.handbook_dir)
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir().join("flow.db")
    }

    pub fn is_https(&self) -> bool {
        self.public_url.starts_with("https://")
    }

    /// `flow.giosuel.io` or `10.0.8.1:8790`: the host part of the public URL.
    pub fn public_host(&self) -> &str {
        let rest = self.public_url.split_once("://").map(|(_, r)| r).unwrap_or(&self.public_url);
        rest.split('/').next().unwrap_or(rest)
    }

    /// The origin browsers send in the Origin header, e.g. `https://flow.giosuel.io`.
    pub fn public_origin(&self) -> String {
        let scheme = if self.is_https() { "https" } else { "http" };
        format!("{scheme}://{}", self.public_host())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_listen_is_refused() {
        let c: Config = toml::from_str("listen = \"0.0.0.0:8790\"").unwrap();
        assert!(c.check().is_err());
        let c: Config = toml::from_str("listen = \"[::]:8790\"").unwrap();
        assert!(c.check().is_err());
        let c: Config = toml::from_str("listen = \"10.0.8.1:8790\"").unwrap();
        assert!(c.check().is_ok());
    }

    #[test]
    fn public_host_and_origin() {
        let c: Config = toml::from_str("public_url = \"https://flow.giosuel.io/\"").unwrap();
        assert_eq!(c.public_host(), "flow.giosuel.io");
        assert_eq!(c.public_origin(), "https://flow.giosuel.io");
        assert!(c.is_https());
    }
}
