//! How `flow` prints (the owner, 2026-11-17: "better formatted and colored output similar to the
//! old viber-run … I also want it to work on narrow terminals … make sure all the `flow` commands'
//! output and the new tui fit in color as well as style and layout").
//!
//! viber-run's palette and layout, in one place for every command and `flow builder status`:
//! names and commands **bold**, details dim, numbers cyan, topics (branches) magenta, commits
//! yellow, what's running or started green (▶ ●), errors red. A line of fields wraps at word
//! boundaries to the terminal's width, continuing indented.

use std::io::IsTerminal;

/// The colours, by what they mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Bold,
    Dim,
    /// Numbers: #12, counts.
    Number,
    /// Started, running, online, done.
    Good,
    /// Branches and topics.
    Topic,
    /// Commits, warnings.
    Commit,
    Bad,
}

impl Tone {
    fn ansi(self) -> &'static str {
        match self {
            Tone::Plain => "",
            Tone::Bold => "\x1b[1m",
            Tone::Dim => "\x1b[2m",
            Tone::Number => "\x1b[36m",
            Tone::Good => "\x1b[32m",
            Tone::Topic => "\x1b[35m",
            Tone::Commit => "\x1b[33m",
            Tone::Bad => "\x1b[31m",
        }
    }
}

/// Colours on stdout (or stderr): only on a terminal, and never with NO_COLOR or TERM=dumb.
pub fn color(stderr: bool) -> bool {
    let tty = if stderr { std::io::stderr().is_terminal() } else { std::io::stdout().is_terminal() };
    tty && std::env::var_os("NO_COLOR").is_none() && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
}

/// `text` in `tone`, when colours are on.
pub fn paint(tone: Tone, text: &str, on: bool) -> String {
    if !on || tone == Tone::Plain || text.is_empty() {
        text.to_owned()
    } else {
        format!("{}{text}\x1b[0m", tone.ansi())
    }
}

/// The terminal's width (COLUMNS, else the terminal's own, else 80), between 40 and 120: wider
/// lines are hard to read.
pub fn width() -> usize {
    let w = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .or_else(|| rustix::termios::tcgetwinsize(std::io::stdout()).ok().map(|s| s.ws_col as usize).filter(|&c| c > 0))
        .or_else(|| rustix::termios::tcgetwinsize(std::io::stderr()).ok().map(|s| s.ws_col as usize).filter(|&c| c > 0))
        .unwrap_or(80);
    w.clamp(40, 120)
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// Fields on a line, two spaces apart, as many as fit; the next ones on lines indented by `more`
/// (viber-run's `fit`). Empty fields are left out.
pub fn fit(indent: &str, more: usize, fields: &[(Tone, &str)], on: bool) -> String {
    let cols = width();
    let mut out = String::from(indent);
    let mut len = chars(indent);
    let mut first = true;
    for (tone, text) in fields.iter().filter(|(_, t)| !t.is_empty()) {
        if !first && len + 2 + chars(text) > cols {
            out.push('\n');
            out.push_str(&" ".repeat(more));
            len = more;
            first = true;
        }
        if !first {
            out.push_str("  ");
            len += 2;
        }
        out.push_str(&paint(*tone, text, on));
        len += chars(text);
        first = false;
    }
    out
}

/// `text` wrapped at word boundaries to the width, each line indented by `indent`.
pub fn wrap(indent: usize, tone: Tone, text: &str, on: bool) -> String {
    let cols = width().saturating_sub(indent).max(20);
    let pad = " ".repeat(indent);
    let mut lines = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split_whitespace() {
            if !line.is_empty() && chars(&line) + 1 + chars(word) > cols {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    lines.into_iter().map(|l| format!("{pad}{}", paint(tone, &l, on))).collect::<Vec<_>>().join("\n")
}

/// A step that went well: "  ✓ approved".
pub fn ok(text: &str) -> String {
    let on = color(true);
    format!("  {} {text}", paint(Tone::Good, "✓", on))
}

/// AgentFlow's own note in a stream of output: "· queued: waiting for its turn", dimmed.
pub fn note(text: &str) -> String {
    paint(Tone::Dim, text, color(true))
}

/// How `flow` reports an error: "flow: what went wrong", the causes below it, indented.
pub fn error(e: &anyhow::Error) -> String {
    let on = color(true);
    let mut out = format!("{} {}", paint(Tone::Bad, "flow:", on), e);
    for cause in e.chain().skip(1) {
        out.push('\n');
        out.push_str(&wrap(2, Tone::Dim, &cause.to_string(), on));
    }
    out
}

/// clap's colours, the same palette: headings bold, commands and flags cyan, placeholders dim.
pub fn clap_styles() -> clap::builder::Styles {
    use clap::builder::styling::{AnsiColor, Effects};
    clap::builder::Styles::styled()
        .header(Effects::BOLD.into())
        .usage(Effects::BOLD.into())
        .literal(AnsiColor::Cyan.on_default())
        .placeholder(Effects::DIMMED.into())
        .error(AnsiColor::Red.on_default() | Effects::BOLD)
        .valid(AnsiColor::Green.on_default())
        .invalid(AnsiColor::Yellow.on_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_wrap_to_the_width_and_text_at_words() {
        // SAFETY: tests that read COLUMNS run in this process only through these functions.
        unsafe { std::env::set_var("COLUMNS", "40") };
        let line = fit(
            "",
            6,
            &[(Tone::Number, "#12"), (Tone::Dim, "2026-11-17 13:40"), (Tone::Topic, "feat/a-long-branch-name")],
            false,
        );
        assert_eq!(line, "#12  2026-11-17 13:40\n      feat/a-long-branch-name");
        assert!(
            wrap(2, Tone::Plain, "one two three four five six seven eight nine ten eleven", false).lines().all(|l| l
                .chars()
                .count()
                <= 40
                && l.starts_with("  "))
        );
        assert_eq!(paint(Tone::Bold, "x", true), "\x1b[1mx\x1b[0m");
        assert_eq!(paint(Tone::Bold, "x", false), "x");
    }
}
