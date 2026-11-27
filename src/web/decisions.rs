//! The Questions tab: questions and decisions as one (the owner, 2026-11-23, Q-6: "i like the idea
//! to merge questions and decisions"; Q-8, D-247: built as drawn on S-38 and S-39; called Questions,
//! 2026-11-24, D-250). The open
//! questions come first, answered in place; under them the decisions, filtered like the Log and
//! Notes (search, Scope, State, time range, Clear) and shown as a timeline by day that quotes the
//! owner. A replaced decision folds under the one that replaced it.
//!
//! The filters come from the address: `dq` (words), `dscope` (scopes picked, `|` between them:
//! `project` or a milestone's name; project-wide and the milestones that aren't closed when left
//! out), `dstate` (in force by default; replaced, archived, all) and `drange` (24h, 7d, 30d); and
//! `dpage`, the page of the timeline, PER_PAGE decisions a page (the owner, 2026-11-27: "there are
//! still very long lists in the app, like the decisions one, can we make that paginated as well?").

use std::collections::HashMap;

use rusqlite::Connection;
use serde_json::Value;

use super::notes::{Choice, marked};
use super::uilib::{Choice as AnswerChoice, Pager};
use crate::records::{self, Kind, Record};
use crate::tz::Tz;
use crate::util::ago;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    pub q: String,
    /// The scopes picked; empty means the default (project-wide and the open milestones).
    pub scopes: Vec<String>,
    pub state: String,
    pub range: String,
    /// The page of the timeline, from 1; 0 or 1 is the first.
    pub page: i64,
}

/// Decisions a page of the timeline.
pub const PER_PAGE: i64 = 20;

impl Filter {
    pub fn new(
        q: Option<&str>,
        scopes: Option<&str>,
        state: Option<&str>,
        range: Option<&str>,
        page: Option<&str>,
    ) -> Self {
        let pick = |v: Option<&str>, ok: &[&str]| v.filter(|v| ok.contains(v)).unwrap_or("").to_owned();
        Filter {
            q: q.unwrap_or("").trim().chars().take(200).collect(),
            scopes: scopes
                .unwrap_or("")
                .split('|')
                .map(str::trim)
                .filter(|s| !s.is_empty() && s.len() <= 40)
                .map(str::to_owned)
                .collect(),
            state: pick(state, &["replaced", "archived", "all"]),
            range: pick(range, &["24h", "7d", "30d"]),
            page: page.and_then(|p| p.trim().parse().ok()).filter(|p| *p > 1).unwrap_or(0),
        }
    }

    /// The tab's address with these filters (and the page, after the first).
    pub fn href(&self, slug: &str) -> String {
        use super::log::enc;
        let mut u = format!("/p/{slug}?tab=questions");
        if !self.q.is_empty() {
            u += &format!("&dq={}", enc(&self.q));
        }
        if !self.scopes.is_empty() {
            u += &format!("&dscope={}", enc(&self.scopes.join("|")));
        }
        for (k, v) in [("dstate", &self.state), ("drange", &self.range)] {
            if !v.is_empty() {
                u += &format!("&{k}={v}");
            }
        }
        if self.page > 1 {
            u += &format!("&dpage={}", self.page);
        }
        u
    }

    /// These filters changed: a change starts again at the first page.
    fn with(&self, f: impl FnOnce(&mut Filter)) -> Filter {
        let mut x = self.clone();
        x.page = 0;
        f(&mut x);
        x
    }

    fn range_ms(&self) -> Option<i64> {
        match self.range.as_str() {
            "24h" => Some(86_400_000),
            "7d" => Some(7 * 86_400_000),
            "30d" => Some(30 * 86_400_000),
            _ => None,
        }
    }

    fn state_ok(&self, r: &Record) -> bool {
        match self.state.as_str() {
            "all" => true,
            "replaced" => r.state == "replaced",
            "archived" => r.state == "archived",
            _ => r.state == "current",
        }
    }
}

/// An open question, whole: every option with its detail, and room for the owner's own words.
pub struct QuestionCard {
    pub short: String,
    pub url: String,
    pub scope: String,
    pub asked: String,
    pub blocks: Vec<(String, String)>,
    pub title: String,
    /// Its details as one line of plain text (the card cuts it at three lines).
    pub body: String,
    pub options: Vec<AnswerChoice>,
}

/// A decision replaced by the one it's folded under.
pub struct Folded {
    pub short: String,
    pub url: String,
    pub title: String,
}

pub struct Entry {
    pub short: String,
    pub url: String,
    pub time: String,
    pub title_html: String,
    /// The owner's words, as they said them; empty when none were recorded.
    pub quote_html: String,
    /// "project-wide" or the milestone's name.
    pub scope: String,
    pub project_wide: bool,
    /// Who said it and where: "you, in chat".
    pub source: String,
    /// The question it answers: (id, address, title).
    pub answers: Option<(String, String, String)>,
    /// The decisions it replaced: (id, address).
    pub replaces: Vec<(String, String)>,
    /// Shown when it isn't in force: "replaced by D-12" or "archived".
    pub not_in_force: String,
    pub folded: Vec<Folded>,
}

/// A day of the timeline: "Today · Mon 23 Nov" and its decisions, newest first.
pub struct Day {
    pub label: String,
    pub entries: Vec<Entry>,
}

pub struct DecisionsView {
    pub questions: Vec<QuestionCard>,
    /// Where an answer comes back to: this tab with its filters.
    pub back: String,
    pub q: String,
    /// Hidden fields that keep the other filters when searching.
    pub keep: Vec<(&'static str, String)>,
    pub scope_label: String,
    /// The Scope menu isn't at its default.
    pub scope_set: bool,
    pub scopes_open: Vec<Choice>,
    pub scopes_closed: Vec<Choice>,
    pub scopes_reset: Option<String>,
    pub state_label: String,
    pub states: Vec<Choice>,
    pub ranges: Vec<Choice>,
    pub reset: Option<String>,
    /// This page's decisions.
    pub days: Vec<Day>,
    /// The timeline's pages, in the card's foot; shown when there's more than one.
    pub pager: Pager,
    /// "79 in project-wide and M14 · newest first".
    pub summary: String,
    /// Under the timeline: what the State filter hides.
    pub foot: String,
    /// The project has no decisions at all.
    pub none: bool,
}

/// "you, in chat" from a decision's `source` ("owner, in chat", "owner, answering Q-25 in
/// AgentFlow", "owner"); another source as it was written.
fn source_line(source: &str) -> String {
    let s = source.trim();
    let lower = s.to_lowercase();
    if s.is_empty() || lower == "owner" {
        return "you".into();
    }
    if lower.starts_with("owner") {
        if lower.contains("chat") {
            return "you, in chat".into();
        }
        if lower.contains("agentflow") {
            return "you, in AgentFlow".into();
        }
        return "you".into();
    }
    s.to_owned()
}

/// When the owner said it: `said_at`, or when it was recorded.
fn said_at(r: &Record) -> i64 {
    r.data.get("said_at").and_then(Value::as_i64).unwrap_or(r.created_at)
}

fn plain(body: &str) -> String {
    body.lines()
        .map(|l| l.trim().trim_start_matches(['#', '>', '-', '*', ' ']).trim())
        .filter(|l| !l.is_empty() && !l.starts_with("```"))
        .collect::<Vec<_>>()
        .join(" ")
        .replace(['`', '*'], "")
}

fn day_label(at: i64, now: i64, tz: &Tz) -> String {
    let (today, day) = (tz.day_start(now), tz.day_start(at));
    let weekday = tz.day_label(at);
    let date = format!("{} {}", weekday.split(' ').next().unwrap_or(""), tz.day(at));
    if day == today {
        format!("Today · {date}")
    } else if day == tz.day_start(today - 1) {
        format!("Yesterday · {date}")
    } else {
        date
    }
}

pub fn view(
    c: &Connection,
    slug: &str,
    project_id: i64,
    f: &Filter,
    now: i64,
    tz: &Tz,
) -> anyhow::Result<DecisionsView> {
    let ms: Vec<_> =
        crate::projects::milestones(c, project_id)?.into_iter().filter(|m| m.parent_id.is_none()).collect();
    let all = records::query(
        c,
        &records::Query {
            project_id: Some(project_id),
            kinds: vec![Kind::Decision, Kind::Question],
            include_archived: true,
            limit: 0,
            ..Default::default()
        },
    )?;
    let by_id: HashMap<i64, &Record> = all.iter().map(|r| (r.id, r)).collect();

    // The open questions, oldest first, as on the overview.
    let mut open: Vec<&Record> = all.iter().filter(|r| r.kind == Kind::Question && r.state == "open").collect();
    open.sort_by_key(|r| r.number);
    let mut questions = Vec::new();
    for q in open {
        let rec = q.data.get("recommended").and_then(Value::as_u64).unwrap_or(0) as usize;
        let options = q
            .data
            .get("options")
            .and_then(Value::as_array)
            .map(|opts| {
                opts.iter()
                    .enumerate()
                    .map(|(i, o)| AnswerChoice {
                        n: i + 1,
                        title: o.get("title").and_then(Value::as_str).unwrap_or("").to_owned(),
                        detail: o.get("detail").and_then(Value::as_str).unwrap_or("").to_owned(),
                        recommended: rec == i + 1,
                    })
                    .collect()
            })
            .unwrap_or_default();
        questions.push(QuestionCard {
            short: q.short_id(),
            url: q.url(),
            scope: q.milestone.clone().unwrap_or_else(|| "project".into()),
            asked: format!("asked {}", ago(q.created_at, now)),
            blocks: crate::waiting::blocks(c, q)?,
            title: q.title.clone(),
            body: plain(&q.body),
            options,
        });
    }

    // The links the timeline shows: which question a decision answers, which decisions it replaced.
    let mut answers: HashMap<i64, i64> = HashMap::new();
    let mut replaced: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut replaced_by: HashMap<i64, i64> = HashMap::new();
    {
        let mut stmt = c.prepare(
            "SELECT l.from_id, l.to_id, l.relation FROM links l JOIN records r ON r.id = l.from_id \
             WHERE r.project_id = ?1 AND r.kind = 'decision' AND l.relation IN ('answers', 'supersedes') \
             ORDER BY l.id",
        )?;
        let rows: Vec<(i64, i64, String)> =
            stmt.query_map([project_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
        for (from, to, relation) in rows {
            if relation == "answers" {
                answers.entry(from).or_insert(to);
            } else if by_id.get(&to).is_some_and(|r| r.kind == Kind::Decision) {
                replaced.entry(from).or_default().push(to);
                replaced_by.insert(to, from);
            }
        }
    }

    let decisions: Vec<&Record> = all.iter().filter(|r| r.kind == Kind::Decision).collect();
    let none = decisions.is_empty();
    let scope_of = |r: &Record| r.milestone.clone().unwrap_or_else(|| "project".into());

    // Scope: project-wide and the milestones that aren't closed, unless picked otherwise.
    let open_ms: Vec<&str> = {
        let mut v: Vec<_> = ms.iter().filter(|m| m.state != "closed").collect();
        v.sort_by(|a, b| b.position.total_cmp(&a.position));
        v.into_iter().map(|m| m.name.as_str()).collect()
    };
    let closed_ms: Vec<&str> = {
        let mut v: Vec<_> = ms.iter().filter(|m| m.state == "closed").collect();
        v.sort_by(|a, b| b.position.total_cmp(&a.position));
        v.into_iter().map(|m| m.name.as_str()).collect()
    };
    let default_scopes: Vec<String> =
        std::iter::once("project".to_owned()).chain(open_ms.iter().map(|s| (*s).to_owned())).collect();
    let picked: Vec<String> = if f.scopes.is_empty() { default_scopes.clone() } else { f.scopes.clone() };
    let scope_ok = |r: &Record| picked.contains(&scope_of(r));

    let words: Vec<String> = f.q.to_lowercase().split_whitespace().map(str::to_owned).collect();
    let search_ok = |r: &Record| {
        if words.is_empty() {
            return true;
        }
        let hay =
            format!("{} {} {} {} {}", r.short_id(), r.title, r.body, r.str("owner_words"), scope_of(r)).to_lowercase();
        words.iter().all(|w| hay.contains(w.as_str()))
    };
    let since = f.range_ms().map(|ms| now - ms);
    let time_ok = |r: &Record| since.is_none_or(|s| said_at(r) >= s);

    // Each menu counts what its own choices would show, with the other filters as they are.
    let count_scope = |name: &str| {
        decisions.iter().filter(|r| scope_of(r) == name && f.state_ok(r) && time_ok(r) && search_ok(r)).count()
    };
    let toggle = |name: &str| {
        let mut next = picked.clone();
        if next.iter().any(|s| s == name) {
            next.retain(|s| s != name);
        } else {
            next.push(name.to_owned());
        }
        f.with(|x| x.scopes = if next == default_scopes { Vec::new() } else { next })
    };
    let scope_choice = |name: &str, label: String| {
        (label, count_scope(name).to_string(), toggle(name).href(slug), picked.iter().any(|s| s == name))
    };
    let ms_label = |name: &str| {
        let title = ms.iter().find(|m| m.name == name).map(|m| m.title.as_str()).unwrap_or("");
        if title.is_empty() { name.to_owned() } else { format!("{name} {title}") }
    };
    let mut scopes_open = vec![scope_choice("project", "Project-wide".into())];
    scopes_open.extend(open_ms.iter().map(|n| scope_choice(n, ms_label(n))));
    // Closed milestones only when they have decisions (or are picked).
    let scopes_closed = closed_ms
        .iter()
        .filter(|n| picked.iter().any(|s| s == *n) || decisions.iter().any(|r| scope_of(r) == **n))
        .map(|n| scope_choice(n, ms_label(n)))
        .collect();
    let scope_label = match picked.len() {
        0 => "None".to_owned(),
        1 if picked[0] == "project" => "Project-wide".to_owned(),
        1 => picked[0].clone(),
        n => format!("{n} picked"),
    };
    let scope_names = |p: &[String]| {
        let names: Vec<String> =
            p.iter().map(|s| if s == "project" { "project-wide".to_owned() } else { s.clone() }).collect();
        match names.len() {
            0 => "no scope".to_owned(),
            1 => names[0].clone(),
            2 => format!("{} and {}", names[0], names[1]),
            n => format!("{} scopes", n),
        }
    };

    let count = |pred: &dyn Fn(&Record) -> bool| decisions.iter().filter(|r| pred(r)).count();
    let states = [("", "In force"), ("replaced", "Replaced"), ("archived", "Archived"), ("all", "All")]
        .into_iter()
        .map(|(k, label)| {
            let g = f.with(|x| x.state = k.into());
            let n = count(&|r| scope_ok(r) && time_ok(r) && search_ok(r) && g.state_ok(r));
            (label.to_owned(), n.to_string(), g.href(slug), f.state == k)
        })
        .collect();
    let ranges = [("24h", "24 h"), ("7d", "7 d"), ("30d", "30 d"), ("", "All")]
        .into_iter()
        .map(|(k, label)| (label.to_owned(), String::new(), f.with(|x| x.range = k.into()).href(slug), f.range == k))
        .collect();

    let mut shown: Vec<&Record> =
        decisions.iter().copied().filter(|r| scope_ok(r) && f.state_ok(r) && time_ok(r) && search_ok(r)).collect();
    shown.sort_by_key(|r| std::cmp::Reverse((said_at(r), r.number)));
    // A page at a time, newest first; its links come back down to the timeline.
    let pager = Pager::new("Pages of the decisions", shown.len() as i64, f.page.max(1), PER_PAGE, |p| {
        let mut g = f.clone();
        g.page = p;
        g.href(slug) + "#decided"
    })
    .labels("Newer decisions", "Older decisions");
    let page_rows = shown.iter().skip(pager.offset(PER_PAGE) as usize).take(PER_PAGE as usize);
    // In force, a replaced decision folds under the one that replaced it.
    let fold = f.state.is_empty();
    let mut days: Vec<(i64, Day)> = Vec::new();
    for r in page_rows {
        let at = said_at(r);
        let words_q = r.str("owner_words");
        let answer = answers.get(&r.id).and_then(|q| by_id.get(q)).map(|q| (q.short_id(), q.url(), q.title.clone()));
        let olds: Vec<&Record> =
            replaced.get(&r.id).map(|v| v.iter().filter_map(|id| by_id.get(id).copied()).collect()).unwrap_or_default();
        let entry = Entry {
            short: r.short_id(),
            url: r.url(),
            time: tz.hm(at),
            title_html: marked(&r.title, &words),
            quote_html: if words_q.trim().is_empty() { String::new() } else { marked(words_q.trim(), &words) },
            scope: r.milestone.clone().unwrap_or_else(|| "project-wide".into()),
            project_wide: r.milestone.is_none(),
            source: source_line(r.str("source")),
            answers: answer,
            replaces: olds.iter().map(|o| (o.short_id(), o.url())).collect(),
            not_in_force: match r.state.as_str() {
                "replaced" => match replaced_by.get(&r.id).and_then(|id| by_id.get(id)) {
                    Some(n) => format!("replaced by {}", n.short_id()),
                    None => "replaced".into(),
                },
                "archived" => "archived".into(),
                _ => String::new(),
            },
            folded: if fold {
                olds.iter()
                    .filter(|o| o.state == "replaced")
                    .map(|o| Folded { short: o.short_id(), url: o.url(), title: o.title.clone() })
                    .collect()
            } else {
                Vec::new()
            },
        };
        let day = tz.day_start(at);
        match days.last_mut() {
            Some((d, x)) if *d == day => x.entries.push(entry),
            _ => days.push((day, Day { label: day_label(at, now, tz), entries: vec![entry] })),
        }
    }

    let filtered = !f.q.is_empty() || !f.range.is_empty();
    let summary = if filtered {
        format!("{} found", shown.len())
    } else {
        format!(
            "{} in {}{} · newest first",
            shown.len(),
            scope_names(&picked),
            match f.state.as_str() {
                "replaced" => ", replaced",
                "archived" => ", archived",
                _ => "",
            }
        )
    };
    let foot = if f.state.is_empty() {
        let hidden = |s: &str| count(&|r| r.state == s && scope_ok(r) && time_ok(r) && search_ok(r));
        let parts: Vec<String> = [("replaced", hidden("replaced")), ("archived", hidden("archived"))]
            .into_iter()
            .filter(|(_, n)| *n > 0)
            .map(|(s, n)| format!("{n} {s}"))
            .collect();
        match (shown.is_empty(), parts.is_empty()) {
            (_, true) if shown.is_empty() && !none => "Nothing matches.".into(),
            (_, true) => String::new(),
            (_, false) => format!("{} hidden: State › Replaced, Archived or All.", parts.join(" and ")),
        }
    } else if shown.is_empty() {
        "Nothing matches.".into()
    } else {
        String::new()
    };
    let mut keep = Vec::new();
    for (k, v) in [("dscope", f.scopes.join("|")), ("dstate", f.state.clone()), ("drange", f.range.clone())] {
        if !v.is_empty() {
            keep.push((k, v));
        }
    }
    Ok(DecisionsView {
        questions,
        back: f.href(slug),
        q: f.q.clone(),
        keep,
        scope_label,
        scope_set: !f.scopes.is_empty(),
        scopes_open,
        scopes_closed,
        scopes_reset: (!f.scopes.is_empty()).then(|| f.with(|x| x.scopes.clear()).href(slug)),
        state_label: match f.state.as_str() {
            "replaced" => "Replaced",
            "archived" => "Archived",
            "all" => "All",
            _ => "In force",
        }
        .into(),
        states,
        ranges,
        reset: (*f != Filter::default()).then(|| Filter::default().href(slug)),
        days: days.into_iter().map(|(_, d)| d).collect(),
        pager,
        summary,
        foot,
        none,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_reads_as_you() {
        assert_eq!(source_line("owner, in chat"), "you, in chat");
        assert_eq!(source_line("owner, answering vinculum/Q-25 in AgentFlow"), "you, in AgentFlow");
        assert_eq!(source_line("owner"), "you");
        assert_eq!(source_line(""), "you");
        assert_eq!(source_line("the consultant"), "the consultant");
    }

    #[test]
    fn filters_round_trip_through_the_address() {
        let f = Filter::new(Some("cookie"), Some("project|M14"), Some("all"), Some("7d"), Some("3"));
        assert_eq!(f.href("x"), "/p/x?tab=questions&dq=cookie&dscope=project%7CM14&dstate=all&drange=7d&dpage=3");
        let g = Filter::new(None, None, Some("bogus"), Some("1y"), Some("x"));
        assert_eq!(g, Filter::default());
        assert_eq!(g.href("x"), "/p/x?tab=questions");
        // The first page has no number; changing a filter starts again at the first page.
        assert_eq!(Filter::new(None, None, None, None, Some("1")).href("x"), "/p/x?tab=questions");
        assert_eq!(
            f.with(|x| x.range.clear()).href("x"),
            "/p/x?tab=questions&dq=cookie&dscope=project%7CM14&dstate=all"
        );
    }
}
