//! Claude Code managed settings that keep the agents' file tools away from AgentFlow's tokens and
//! data (the owner, 2026-11-07: "managed-settings deny rules for the token folder and
//! ~/.claude.json (owner runs the sudo command)"). They only stop Claude's own Read and Edit
//! tools; a shell command run as the same user can still read the files, so this is a fence,
//! not a wall. Separate Unix users would be the wall.

use crate::config::Config;

pub const PATH: &str = "/etc/claude-code/managed-settings.json";

/// The settings: deny reading and editing AgentFlow's tokens, its data and its config, and
/// Claude Code's own config where MCP servers are registered.
pub fn managed_settings(config: &Config) -> serde_json::Value {
    let home = crate::util::home_dir();
    let rel = |p: std::path::PathBuf| match p.strip_prefix(&home) {
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => format!("/{}", p.display()),
    };
    let mut deny = Vec::new();
    for dir in [config.token_dir(), config.data_dir()] {
        let d = rel(dir);
        deny.push(format!("Read({d}/**)"));
        deny.push(format!("Edit({d}/**)"));
    }
    for file in ["~/.config/agentflow/config.toml", "~/.claude.json"] {
        deny.push(format!("Read({file})"));
        deny.push(format!("Edit({file})"));
    }
    serde_json::json!({ "permissions": { "deny": deny } })
}

/// Writes the settings to a file in the data folder and returns the text `agentflow
/// managed-settings` prints: the settings, and the root command for the owner to run (fish works
/// too). The command only names files, so nothing in the config ends up inside a root command.
pub fn managed_settings_command(config: &Config) -> anyhow::Result<String> {
    let json = serde_json::to_string_pretty(&managed_settings(config))?;
    let file = config.data_dir().join("managed-settings.json");
    let plain = file.to_string_lossy().chars().all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c));
    if !plain {
        anyhow::bail!("the data folder's path has unusual characters; copy the settings below by hand:\n{json}");
    }
    std::fs::write(&file, format!("{json}\n"))?;
    let mut out = format!(
        "These settings stop every Claude Code session's Read and Edit tools on AgentFlow's files:\n\n{json}\n\n"
    );
    if std::path::Path::new(PATH).exists() {
        out.push_str(&format!(
            "{PATH} exists already. Add the \"deny\" entries above to its permissions by hand:\n\n  sudoedit {PATH}\n"
        ));
    } else {
        out.push_str(&format!(
            "They're in {}. Install them (needs root; run it yourself):\n\n  sudo install -D -m 644 {} {PATH}\n\nSessions pick them up when they start.\n",
            file.display(),
            file.display()
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn denies_tokens_data_and_claude_config() {
        let c = crate::config::Config::default();
        let v = super::managed_settings(&c).to_string();
        for want in ["Read(~/.config/agentflow/tokens/**)", "Edit(~/.local/state/agentflow/**)", "Read(~/.claude.json)"]
        {
            assert!(v.contains(want), "{want} in {v}");
        }
        let dir = tempfile::tempdir().unwrap();
        let c = crate::config::Config { data_dir: dir.path().display().to_string(), ..c };
        let text = super::managed_settings_command(&c).unwrap();
        assert!(text.contains("sudo") && !text.contains("echo '"), "the command names a file: {text}");
        assert!(dir.path().join("managed-settings.json").exists());
    }
}
