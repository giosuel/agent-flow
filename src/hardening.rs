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

/// The text `agentflow admin managed-settings` prints: the settings, and the root commands for
/// the owner to run (fish works too).
pub fn managed_settings_command(config: &Config) -> String {
    let json = serde_json::to_string_pretty(&managed_settings(config)).unwrap_or_default();
    let exists = std::path::Path::new(PATH).exists();
    let mut out = format!(
        "These settings stop every Claude Code session's Read and Edit tools on AgentFlow's files:\n\n{json}\n\n"
    );
    if exists {
        out.push_str(&format!(
            "{PATH} exists already. Add the \"deny\" entries above to its permissions by hand:\n\n  sudoedit {PATH}\n"
        ));
    } else {
        let one_line = serde_json::to_string(&managed_settings(config)).unwrap_or_default();
        out.push_str(&format!(
            "Install them (needs root; run it yourself):\n\n  echo '{one_line}' | sudo install -D -m 644 /dev/stdin {PATH}\n\nSessions pick them up when they start.\n"
        ));
    }
    out
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
        assert!(super::managed_settings_command(&c).contains("sudo"));
    }
}
