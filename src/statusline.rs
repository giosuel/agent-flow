//! `flow statusline` (the owner, 2026-11-17, M13): Claude Code's status line for a project's
//! sessions. It prints the plan's windows ("5h 42% · resets 22:50 · week 71%") and sends them to
//! AgentFlow at most once a minute, from a detached copy, so the line never waits on the network.
//! Claude Code gives it a JSON object on stdin; `rate_limits` is there for subscribers once the
//! session has had its first reply.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::bail;
use serde_json::{Value, json};

use crate::cli::{Client, ClientArgs};
use crate::util::expand_home;

/// How often a sample goes out, from all sessions of this user together.
const EVERY: Duration = Duration::from_secs(60);

fn stamp() -> PathBuf {
    expand_home("~/.local/state/agentflow/statusline-sent")
}

/// The line, from what Claude Code sends: the windows, then the model.
pub fn line(input: &Value, color: bool) -> String {
    let dim = |s: &str| crate::ui::paint(crate::ui::Tone::Dim, s, color);
    let num = |s: &str| crate::ui::paint(crate::ui::Tone::Number, s, color);
    let warn = |s: &str| crate::ui::paint(crate::ui::Tone::Commit, s, color);
    let pct = |w: &Value| -> Option<(f64, i64)> { Some((w["used_percentage"].as_f64()?, w["resets_at"].as_i64()?)) };
    let shown = |p: f64| if p >= 80.0 { warn(&format!("{p:.0}%")) } else { num(&format!("{p:.0}%")) };
    let mut parts = Vec::new();
    let rl = &input["rate_limits"];
    if let Some((p, reset)) = pct(&rl["five_hour"]) {
        parts.push(format!("{} {} {}", dim("5h"), shown(p), dim(&format!("· resets {}", local_hm(reset)))));
    }
    if let Some((p, _)) = pct(&rl["seven_day"]) {
        parts.push(format!("{} {}", dim("week"), shown(p)));
    }
    if let Some(m) = input["model"]["display_name"].as_str().filter(|m| !m.is_empty()) {
        parts.push(dim(m));
    }
    parts.join(&dim(" · "))
}

/// "22:50" in this machine's time zone.
fn local_hm(unix_s: i64) -> String {
    jiff::Timestamp::from_second(unix_s)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M").to_string())
        .unwrap_or_default()
}

/// `flow statusline`. `send` (internal): the detached copy's sample.
pub fn run(config_path: &Path, args: &ClientArgs, send: Option<String>) -> anyhow::Result<()> {
    if let Some(sample) = send {
        let here = std::env::current_dir().unwrap_or_default();
        let client = Client::for_project(config_path, args, &here)?.with_timeout(Duration::from_secs(10));
        let limits: Value = serde_json::from_str(&sample)?;
        client
            .post_json("/api/usage/plan", &json!({"rate_limits": limits, "machine": crate::connect::machine_name()}))?;
        return Ok(());
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::Read::take(std::io::stdin(), 1024 * 1024), &mut text)?;
    let input: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    // Claude Code shows the line in color itself; NO_COLOR still turns it off.
    println!("{}", line(&input, std::env::var_os("NO_COLOR").is_none()));
    let limits = &input["rate_limits"];
    if limits.is_object() && due() {
        let _ = touch();
        let _ = detach(args, &limits.to_string());
    }
    Ok(())
}

fn due() -> bool {
    match std::fs::metadata(stamp()).and_then(|m| m.modified()) {
        Ok(t) => t.elapsed().map(|e| e >= EVERY).unwrap_or(true),
        Err(_) => true,
    }
}

fn touch() -> std::io::Result<()> {
    let p = stamp();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, "")
}

/// `flow statusline --send …` in its own session, without the status line's pipes.
fn detach(args: &ClientArgs, sample: &str) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("statusline");
    if let Some(u) = &args.url {
        cmd.arg("--url").arg(u);
    }
    if let Some(t) = &args.token_file {
        cmd.arg("--token-file").arg(t);
    }
    cmd.arg("--send").arg(sample);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::setsid().map_err(std::io::Error::from)?;
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(_) => Ok(()),
        Err(e) => bail!("starting the send: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_shows_both_windows_and_the_model() {
        let input = json!({
            "model": {"display_name": "Opus 5.5"},
            "rate_limits": {"five_hour": {"used_percentage": 42.4, "resets_at": 1763419800}, "seven_day": {"used_percentage": 71, "resets_at": 1763420400}}
        });
        let l = line(&input, false);
        assert!(l.starts_with("5h 42% · resets ") && l.ends_with(" · week 71% · Opus 5.5"), "{l}");
        assert_eq!(line(&json!({"model": {"display_name": "Opus 5.5"}}), false), "Opus 5.5", "before the first reply");
    }
}
