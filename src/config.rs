//! Server configuration, read from `~/.config/agentflow/config.toml`. Every field has a default, so
//! the file is optional.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::util::expand_home;

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
    /// The contact push services ask senders for (Apple requires one): mailto: or https:.
    pub push_contact: String,
    /// Owner sessions end after this many days without a request…
    pub session_idle_days: i64,
    /// …and after this many days in any case.
    pub session_max_days: i64,
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
            trusted_proxies: vec!["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()],
            session_idle_days: 14,
            session_max_days: 30,
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        expand_home("~/.config/agentflow/config.toml")
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
