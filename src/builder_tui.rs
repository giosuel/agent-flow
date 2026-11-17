//! `flow builder status [name]` (the owner, 2026-11-17: "a builder ncurses tui status page. so i
//! can follow the build steps and usage of the builder machine live"; the text mockup they
//! approved): the machine's CPU (yours and the builds'), memory, GPU and disk as bars, its jobs,
//! and the output of the one picked, live. From any connected machine: everything comes from the
//! server. In the colours of the rest of `flow` (crate::ui).
//!
//! Keys: ↑↓ (or j k) pick a job, ⏎ follows its output or holds it still, p pauses or resumes the
//! builds there, q (or Esc) quits.

use std::time::{Duration, Instant};

use anyhow::Context;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use serde_json::{Value, json};

use crate::cli::{Client, ClientArgs};
use crate::ui::Tone;

/// The palette of `flow`'s other output.
fn st(t: Tone) -> Style {
    match t {
        Tone::Plain => Style::default(),
        Tone::Bold => Style::default().add_modifier(Modifier::BOLD),
        Tone::Dim => Style::default().fg(Color::DarkGray),
        Tone::Number => Style::default().fg(Color::Cyan),
        Tone::Good => Style::default().fg(Color::Green),
        Tone::Topic => Style::default().fg(Color::Magenta),
        Tone::Commit => Style::default().fg(Color::Yellow),
        Tone::Bad => Style::default().fg(Color::Red),
    }
}

struct App {
    name: String,
    status: Value,
    error: Option<String>,
    picked: usize,
    /// The job whose output shows, and its lines so far.
    job: Option<i64>,
    lines: Vec<String>,
    after: u64,
    follow: bool,
    /// Lines up from the bottom when not following.
    scroll: usize,
}

impl App {
    fn jobs(&self) -> Vec<Value> {
        self.status["jobs"].as_array().cloned().unwrap_or_default()
    }
}

pub fn run(config_path: &std::path::Path, args: &ClientArgs, name: Option<String>) -> anyhow::Result<()> {
    let client = Client::new(config_path, args)?.with_timeout(Duration::from_secs(10));
    let name = match name {
        Some(n) => n,
        None => {
            // The only builder, or the one on this machine.
            let list = client.get("/api/builders")?;
            let all: Vec<String> = list["builders"]
                .as_array()
                .map(|a| a.iter().filter_map(|b| b["name"].as_str().map(str::to_owned)).collect())
                .unwrap_or_default();
            let host = crate::machine::hostname();
            match all.as_slice() {
                [one] => one.clone(),
                _ => all
                    .iter()
                    .find(|n| n.eq_ignore_ascii_case(&host))
                    .cloned()
                    .with_context(|| format!("which builder? flow builder status <name>: {}", all.join(", ")))?,
            }
        }
    };
    // Checked before the screen is taken over, so a wrong name is a plain error.
    let first = client.get(&format!("/api/builders/status/{name}"))?;
    let mut app = App {
        name: first["name"].as_str().unwrap_or(&name).to_owned(),
        status: first,
        error: None,
        picked: 0,
        job: None,
        lines: Vec::new(),
        after: 0,
        follow: true,
        scroll: 0,
    };
    // The running job first, if there is one.
    app.picked = app.jobs().iter().position(|j| j["state"] == "running").unwrap_or(0);
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &client, &mut app);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, client: &Client, app: &mut App) -> anyhow::Result<()> {
    let mut last_status = Instant::now();
    let mut last_output = Instant::now() - Duration::from_secs(5);
    loop {
        // The job picked: its output from the start when it changes.
        let picked_id = app.jobs().get(app.picked).and_then(|j| j["id"].as_i64());
        if picked_id != app.job {
            app.job = picked_id;
            app.lines.clear();
            app.after = 0;
            app.scroll = 0;
            last_output = Instant::now() - Duration::from_secs(5);
        }
        if last_status.elapsed() > Duration::from_secs(2) {
            match client.get(&format!("/api/builders/status/{}", app.name)) {
                Ok(s) => {
                    // Keep the same job picked when the list moves.
                    if let Some(id) = app.job
                        && let Some(i) = s["jobs"].as_array().and_then(|a| a.iter().position(|j| j["id"] == id))
                    {
                        app.picked = i;
                    }
                    app.status = s;
                    app.error = None;
                }
                Err(e) => app.error = Some(format!("{e:#}")),
            }
            last_status = Instant::now();
        }
        if let Some(id) = app.job
            && last_output.elapsed() > Duration::from_secs(1)
        {
            // Everything at once the first time; then what's new.
            for _ in 0..20 {
                match client.get(&format!("/api/remote/jobs/{id}/output?after={}", app.after)) {
                    Ok(o) => {
                        let text = o["text"].as_str().unwrap_or("");
                        app.lines.extend(text.lines().map(str::to_owned));
                        app.after = o["next"].as_u64().unwrap_or(app.after);
                        if text.is_empty() {
                            break;
                        }
                    }
                    Err(e) => {
                        app.error = Some(format!("{e:#}"));
                        break;
                    }
                }
            }
            last_output = Instant::now();
        }
        terminal.draw(|f| draw(f, app))?;
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
        {
            let n = app.jobs().len();
            match k.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                KeyCode::Down | KeyCode::Char('j') if n > 0 => app.picked = (app.picked + 1).min(n - 1),
                KeyCode::Up | KeyCode::Char('k') => app.picked = app.picked.saturating_sub(1),
                KeyCode::Enter => {
                    app.follow = !app.follow;
                    app.scroll = 0;
                }
                KeyCode::PageUp => {
                    app.follow = false;
                    app.scroll = (app.scroll + 10).min(app.lines.len());
                }
                KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(10),
                KeyCode::Char('p') => {
                    let paused = app.status["paused"].as_bool().unwrap_or(false);
                    match client.post_json(&format!("/api/builders/pause/{}", app.name), &json!({"paused": !paused})) {
                        Ok(_) => app.status["paused"] = json!(!paused),
                        Err(e) => app.error = Some(format!("{e:#}")),
                    }
                }
                _ => {}
            }
        }
    }
}

/// A bar: `parts` filled in their colours, the rest empty, then the numbers.
fn bar(label: &str, width: u16, parts: &[(f64, Tone)], total: f64, text: Vec<Span<'static>>) -> Line<'static> {
    let cells = width.saturating_sub(6 + 2).clamp(10, 40) as usize;
    let mut spans = vec![Span::styled(format!(" {label:<5}"), st(Tone::Bold))];
    let mut used = 0usize;
    for (v, tone) in parts {
        let n = if total > 0.0 { ((v / total) * cells as f64).round() as usize } else { 0 };
        let n = n.min(cells - used);
        spans.push(Span::styled("█".repeat(n), st(*tone)));
        used += n;
    }
    spans.push(Span::styled("░".repeat(cells - used), st(Tone::Dim)));
    spans.push(Span::raw("  "));
    spans.extend(text);
    Line::from(spans)
}

fn line_tone(l: &str) -> Tone {
    if l.starts_with("· ") {
        Tone::Dim
    } else {
        match crate::remote::line_severity(l) {
            "error" => Tone::Bad,
            "warn" => Tone::Commit,
            _ => Tone::Plain,
        }
    }
}

fn state_tone(s: &str) -> Tone {
    match s {
        "running" => Tone::Good,
        "queued" | "paused" => Tone::Commit,
        "failed" => Tone::Bad,
        _ => Tone::Dim,
    }
}

fn dur(secs: Option<i64>) -> String {
    match secs {
        Some(s) if s >= 3600 => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
        Some(s) => format!("{}m {:02}s", s / 60, s % 60),
        None => String::new(),
    }
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let s = &app.status;
    let p = &s["probe"];
    let num = |k: &str| p[k].as_f64().unwrap_or(0.0);
    let jobs = app.jobs();
    let narrow = area.width < 90;
    let [head, bars, sep1, list_h, sep2, out, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Length(1),
        Constraint::Length((jobs.len().clamp(1, 8) + 1) as u16),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);

    // The header: the machine, online or not, and the keys.
    let online = s["online"].as_bool().unwrap_or(false);
    let paused = s["paused"].as_bool().unwrap_or(false);
    let mut h = vec![
        Span::styled(format!(" {}", app.name), st(Tone::Bold)),
        Span::styled(if online { " ● online" } else { " ○ offline" }, st(if online { Tone::Good } else { Tone::Bad })),
    ];
    if paused {
        h.push(Span::styled(" · builds paused", st(Tone::Commit)));
    }
    if let Some(v) = s["version"].as_str() {
        h.push(Span::styled(format!(" · builder {}", v.rsplit('+').next().unwrap_or(v)), st(Tone::Dim)));
    }
    if !narrow {
        h.push(Span::styled(format!(" · {}", p["model"].as_str().unwrap_or("")), st(Tone::Dim)));
    }
    let keys = Line::from(Span::styled("q quit · ↑↓ job · ⏎ follow · p pause ", st(Tone::Dim)));
    f.render_widget(Paragraph::new(Line::from(h)), head);
    if !narrow {
        f.render_widget(Paragraph::new(keys).right_aligned(), head);
    }

    // The bars.
    let threads = num("threads");
    let (busy, builds) = (num("busy"), num("builds"));
    let yours = (busy - builds).max(0.0);
    let mem_total = num("mem_total_gb");
    let mem_used = (mem_total - num("mem_free_gb")).max(0.0);
    let mem_builds = num("mem_builds_gb").min(mem_used);
    let disk_total = num("disk_total_gb");
    let disk_used = (disk_total - num("disk_free_gb")).max(0.0);
    let w = bars.width;
    let pct = |a: f64, b: f64| if b > 0.0 { format!("{:>3.0}%", a / b * 100.0) } else { "  –".into() };
    let mut lines = vec![
        bar(
            "CPU",
            w / 2,
            &[(yours, Tone::Number), (builds, Tone::Good)],
            threads,
            vec![
                Span::raw(pct(busy, threads)),
                Span::styled(
                    format!("  you {:.0} · builds {:.0} of {threads:.0} threads", yours, builds),
                    st(Tone::Dim),
                ),
            ],
        ),
        bar(
            "MEM",
            w / 2,
            &[(mem_used - mem_builds, Tone::Number), (mem_builds, Tone::Good)],
            mem_total,
            vec![
                Span::raw(format!("{mem_used:.0} / {mem_total:.0} GB")),
                Span::styled(format!("  builds {mem_builds:.0} GB"), st(Tone::Dim)),
            ],
        ),
    ];
    if !p["gpu_name"].as_str().unwrap_or("").is_empty() {
        lines.push(bar(
            "GPU",
            w / 2,
            &[(num("gpu_util"), Tone::Topic)],
            100.0,
            vec![
                Span::raw(format!("{:>3.0}%", num("gpu_util"))),
                Span::styled(format!("  {}", p["gpu_name"].as_str().unwrap_or("")), st(Tone::Dim)),
            ],
        ));
    }
    lines.push(bar(
        "DISK",
        w / 2,
        &[(disk_used, Tone::Commit)],
        disk_total,
        vec![Span::raw(format!("{disk_used:.0} / {disk_total:.0} GB"))],
    ));
    f.render_widget(Paragraph::new(lines), bars);
    let rule = |r: Rect| Paragraph::new(Span::styled("─".repeat(r.width as usize), st(Tone::Dim)));
    f.render_widget(rule(sep1), sep1);
    f.render_widget(rule(sep2), sep2);

    // The jobs.
    let running = jobs.iter().filter(|j| j["state"] == "running").count();
    let waiting = jobs.iter().filter(|j| j["state"] == "queued").count();
    let mut rows = vec![Line::from(vec![
        Span::styled(" JOBS", st(Tone::Bold)),
        Span::styled(format!("   {running} running · {waiting} waiting"), st(Tone::Dim)),
    ])];
    let start = app.picked.saturating_sub(7);
    for (i, j) in jobs.iter().enumerate().skip(start).take(8) {
        let state = j["state"].as_str().unwrap_or("");
        let mark = if i == app.picked { "▶" } else { " " };
        let cmd_w = if narrow { 18 } else { 34 };
        let cmd: String = j["command"].as_str().unwrap_or("").chars().take(cmd_w).collect();
        let mut spans = vec![
            Span::styled(format!(" {mark} "), st(Tone::Good)),
            Span::styled(format!("{:<6}", format!("#{}", j["id"].as_i64().unwrap_or(0))), st(Tone::Number)),
            Span::styled(format!("{:<13}", j["project"].as_str().unwrap_or("")), st(Tone::Bold)),
            Span::raw(format!("{cmd:<cmd_w$}  ")),
            Span::styled(format!("{state:<9}"), st(state_tone(state))),
            Span::styled(format!("{:>8}", dur(j["seconds"].as_i64())), st(Tone::Dim)),
        ];
        if !narrow {
            let detail = match state {
                "queued" | "paused" => j["reason"].as_str().unwrap_or("").to_owned(),
                "running" => format!("{} threads · slot {}", j["threads"], j["slot"]),
                _ => j["exit_code"].as_i64().map(|c| format!("exit {c}")).unwrap_or_default(),
            };
            spans.push(Span::styled(format!("   {detail}"), st(Tone::Dim)));
        }
        let mut line = Line::from(spans);
        if i == app.picked {
            line = line.style(Style::default().bg(Color::Rgb(28, 21, 48)));
        }
        rows.push(line);
    }
    if jobs.is_empty() {
        rows.push(Line::from(Span::styled("   no jobs yet", st(Tone::Dim))));
    }
    f.render_widget(Paragraph::new(rows), list_h);

    // The picked job's output.
    let picked = jobs.get(app.picked);
    let title = picked
        .map(|j| {
            Line::from(vec![
                Span::styled(format!(" #{}", j["id"]), st(Tone::Number)),
                Span::styled(format!(" · {}", j["project"].as_str().unwrap_or("")), st(Tone::Bold)),
                Span::raw(format!(" · {}", j["command"].as_str().unwrap_or(""))),
                Span::styled(
                    if j["commit"].as_str().unwrap_or("").is_empty() {
                        String::new()
                    } else {
                        format!(" · at {}", j["commit"].as_str().unwrap_or(""))
                    },
                    st(Tone::Commit),
                ),
            ])
        })
        .unwrap_or_default();
    let [out_title, out_body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(out);
    f.render_widget(Paragraph::new(title), out_title);
    let height = out_body.height as usize;
    let end = app.lines.len().saturating_sub(if app.follow { 0 } else { app.scroll });
    let from = end.saturating_sub(height);
    let shown: Vec<Line> =
        app.lines[from..end].iter().map(|l| Line::from(Span::styled(format!("   {l}"), st(line_tone(l))))).collect();
    f.render_widget(Paragraph::new(shown).wrap(Wrap { trim: false }), out_body);

    // The foot: following or held, the counts, an error if the server didn't answer.
    let warn = app.lines.iter().filter(|l| line_tone(l) == Tone::Commit).count();
    let err = app.lines.iter().filter(|l| line_tone(l) == Tone::Bad).count();
    let mut foot_spans = vec![
        Span::styled(" ▌", st(Tone::Good)),
        Span::styled(if app.follow { "following" } else { "held (⏎ follows)" }, st(Tone::Plain)),
        Span::styled(format!(" · {} lines · {warn} warnings · {err} errors", app.lines.len()), st(Tone::Dim)),
    ];
    if let Some(e) = &app.error {
        foot_spans.push(Span::styled(format!("   {e}"), st(Tone::Bad)));
    }
    if narrow {
        foot_spans.push(Span::styled("   q ↑↓ ⏎ p", st(Tone::Dim)));
    }
    f.render_widget(Paragraph::new(Line::from(foot_spans)), foot);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> App {
        let status = json!({
            "name": "HEADQUARTERS", "online": true, "paused": false, "version": "0.0.0-dev+g92b2be2", "slots": 3,
            "probe": {"model": "AMD Ryzen 9 9950X3D 16-Core Processor", "threads": 32.0, "busy": 13.0, "builds": 9.0,
                      "mem_total_gb": 92.0, "mem_free_gb": 61.0, "mem_builds_gb": 18.0, "gpu_name": "NVIDIA GeForce RTX 5090",
                      "gpu_util": 4.0, "disk_free_gb": 518.0, "disk_total_gb": 930.0},
            "jobs": [
                {"id": 24, "project": "imaginarium", "command": "cargo test --workspace", "state": "running", "reason": "",
                 "threads": 14, "slot": 1, "seconds": 134, "commit": "84ac88f"},
                {"id": 25, "project": "imaginarium", "command": "cargo clippy --all-targets", "state": "queued",
                 "reason": "3 jobs of this project are running (at most 3 at once)", "seconds": null, "commit": ""},
                {"id": 23, "project": "agentflow", "command": "cargo test -q", "state": "done", "exit_code": 0, "seconds": 51, "commit": "92b2be2"}
            ]
        });
        App {
            name: "HEADQUARTERS".into(),
            status,
            error: None,
            picked: 0,
            job: Some(24),
            lines: vec![
                "· 13:40:02 copying the snapshot to HEADQUARTERS".into(),
                "   Compiling imaginarium-core v0.9.3".into(),
                "warning: unused variable `scale`".into(),
                "test render::tests::golden_frames ... ok".into(),
            ],
            after: 0,
            follow: true,
            scroll: 0,
        }
    }

    fn render(width: u16, height: u16) -> String {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let app = sample();
        t.draw(|f| draw(f, &app)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol().to_owned()).collect::<String>().trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_screen_shows_the_machine_jobs_and_output_wide_and_narrow() {
        let wide = render(120, 24);
        println!("{wide}");
        assert!(wide.contains("HEADQUARTERS ● online") && wide.contains("q quit"), "{wide}");
        assert!(wide.contains("CPU") && wide.contains("you 4 · builds 9 of 32 threads"), "{wide}");
        assert!(wide.contains("#24") && wide.contains("running") && wide.contains("slot 1"), "{wide}");
        assert!(wide.contains("golden_frames") && wide.contains("following · 4 lines · 1 warnings"), "{wide}");
        let narrow = render(64, 24);
        println!("{narrow}");
        assert!(narrow.lines().all(|l| l.chars().count() <= 64));
        assert!(narrow.contains("#25") && narrow.contains("q ↑↓ ⏎ p"), "{narrow}");
    }
}
