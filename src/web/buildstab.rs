//! The Builds tab with To test merged in (the owner, 2026-11-23, Q-6: "merge … the build tab";
//! Q-8, D-247: built as drawn on S-40 to S-47). The newest build leads, with its platforms, what it
//! needs, the command that starts it and a download; then what's new in it, answered right there
//! (Works, Problem, Skip); then everything else still to try, by category, under the app's filter
//! band; then the earlier builds in the table component.
//!
//! A build published for several platforms is several records with one commit: they show as one
//! build with a platform each. The filters come from the address: `tq` (words), `tcat`
//! (categories, `|` between them), `tfrom` (a build, e.g. B-66, or "none") and `tstate` (open by
//! default; problem, tested, skipped, all). `ball` shows every earlier build.

use std::collections::HashMap;

use rusqlite::Connection;
use serde_json::Value;

use super::notes::Choice;
use super::totest::{Context, Group, Item, grouped, item};
use crate::records::{self, Kind, Query, Record};
use crate::tz::Tz;
use crate::util::ago;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    pub q: String,
    pub cats: Vec<String>,
    pub from: String,
    pub state: String,
    /// Every earlier build, not only the latest few.
    pub all_builds: bool,
}

impl Filter {
    pub fn new(q: Option<&str>, cats: Option<&str>, from: Option<&str>, state: Option<&str>, all: bool) -> Self {
        let from = from.unwrap_or("").trim();
        let from = if from == "none"
            || (from.starts_with("B-") && from[2..].bytes().all(|b| b.is_ascii_digit()) && from.len() > 2)
        {
            from.to_owned()
        } else {
            String::new()
        };
        Filter {
            q: q.unwrap_or("").trim().chars().take(200).collect(),
            cats: cats.unwrap_or("").split('|').map(str::trim).filter(|c| !c.is_empty()).map(str::to_owned).collect(),
            from,
            state: state.filter(|s| ["problem", "tested", "skipped", "all"].contains(s)).unwrap_or("").to_owned(),
            all_builds: all,
        }
    }

    /// The tab's address with these filters.
    pub fn href(&self, slug: &str) -> String {
        use super::log::enc;
        let mut u = format!("/p/{slug}?tab=builds");
        if !self.q.is_empty() {
            u += &format!("&tq={}", enc(&self.q));
        }
        if !self.cats.is_empty() {
            u += &format!("&tcat={}", enc(&self.cats.join("|")));
        }
        for (k, v) in [("tfrom", &self.from), ("tstate", &self.state)] {
            if !v.is_empty() {
                u += &format!("&{k}={v}");
            }
        }
        if self.all_builds {
            u += "&ball=1";
        }
        u
    }

    fn with(&self, f: impl FnOnce(&mut Filter)) -> Filter {
        let mut x = self.clone();
        f(&mut x);
        x
    }

    /// Any of the list's own filters set (the earlier builds' Show all isn't one).
    fn filtering(&self) -> bool {
        !self.q.is_empty() || !self.cats.is_empty() || !self.from.is_empty() || !self.state.is_empty()
    }

    fn state_ok(&self, r: &Record) -> bool {
        let problems = r.data.get("problems").and_then(Value::as_array).is_some_and(|p| !p.is_empty());
        match self.state.as_str() {
            "all" => true,
            "problem" => r.state == "open" && problems,
            "tested" => r.state == "tested",
            "skipped" => r.state == "dropped",
            _ => r.state == "open",
        }
    }
}

/// One platform of a build, to download.
pub struct Platform {
    /// "linux-x86_64".
    pub name: String,
    /// "linux".
    pub os: String,
    pub size: String,
    pub href: String,
    /// "the build's archive" or the one file it is, e.g. "Imaginarium.exe".
    pub what: String,
}

pub struct Newest {
    pub short: String,
    pub url: String,
    pub title: String,
    pub name: String,
    pub when: String,
    pub by: String,
    pub platforms: Vec<Platform>,
    pub needs: Vec<String>,
    /// "flow run imaginarium 67" when there's a build flow run starts.
    pub run: String,
}

pub struct Earlier {
    pub short: String,
    pub url: String,
    pub number: i64,
    pub title: String,
    pub name: String,
    pub platforms: String,
    pub published: String,
    pub published_at: i64,
    /// (tone, text): ("ok", "1 works"), ("bad", "2 problems"); empty: not tried.
    pub feedback: Vec<(&'static str, String)>,
    /// For sorting: how many verdicts it has.
    pub feedback_n: usize,
    pub run: String,
    pub download: String,
}

pub struct BuildsView {
    pub newest: Option<Newest>,
    /// Open items that came with the newest build.
    pub new_items: Vec<Item>,
    /// Everything that came with it, and how many of those were tried.
    pub new_total: usize,
    pub new_tried: usize,
    pub back: String,
    pub is_owner: bool,
    pub q: String,
    pub keep: Vec<(&'static str, String)>,
    pub cat_label: String,
    pub cats: Vec<Choice>,
    pub cats_clear: Option<String>,
    pub from_label: String,
    pub froms: Vec<Choice>,
    pub state_label: String,
    pub states: Vec<Choice>,
    pub reset: Option<String>,
    /// The list under the filters: "Still to try", or "Found" while filtering.
    pub list_title: String,
    pub list_sub: String,
    pub groups: Vec<Group>,
    /// While filtering every group is open; otherwise the first.
    pub filtering: bool,
    pub earlier: Vec<Earlier>,
    pub earlier_total: usize,
    pub earlier_all: Option<String>,
    pub none: bool,
}

fn size_of(r: &Record) -> String {
    let size = r.data.get("size").and_then(Value::as_u64).unwrap_or(0);
    if size >= 1_048_576 {
        format!("{:.1} MB", size as f64 / 1_048_576.0)
    } else {
        format!("{} KB", size.div_ceil(1024))
    }
}

fn platform_of(r: &Record) -> String {
    if r.str("platform").is_empty() { crate::builds::DEFAULT_PLATFORM.to_owned() } else { r.str("platform").to_owned() }
}

fn platform(r: &Record) -> Platform {
    let name = platform_of(r);
    let files = r.data.get("files_count").and_then(Value::as_u64).unwrap_or(0);
    let entry = r.str("entry");
    let file = entry.rsplit('/').next().unwrap_or(entry);
    Platform {
        os: name.split('-').next().unwrap_or(&name).to_owned(),
        name,
        size: size_of(r),
        href: format!("{}/download", r.url()),
        what: if files == 1 && !file.is_empty() { file.to_owned() } else { "the build's archive".to_owned() },
    }
}

/// The builds as the owner sees them: records of one commit together, newest first; each group
/// newest record first.
fn groups_of(builds: &[&Record]) -> Vec<Vec<Record>> {
    let mut out: Vec<Vec<Record>> = Vec::new();
    for b in builds.iter().rev() {
        let commit = b.str("commit");
        match out.iter_mut().find(|g| !commit.is_empty() && g[0].str("commit") == commit) {
            Some(g) => g.push((*b).clone()),
            None => out.push(vec![(*b).clone()]),
        }
    }
    out
}

/// The record flow run starts (Linux), else the group's newest.
fn primary(g: &[Record]) -> &Record {
    g.iter().find(|r| platform_of(r) == crate::builds::DEFAULT_PLATFORM).unwrap_or(&g[0])
}

#[allow(clippy::too_many_arguments)]
pub fn view(
    c: &Connection,
    slug: &str,
    project_id: i64,
    f: &Filter,
    is_owner: bool,
    windows_first: bool,
    now: i64,
    tz: &Tz,
) -> anyhow::Result<BuildsView> {
    let builds = super::totest::builds_oldest_first(c, project_id)?;
    let refs: Vec<&Record> = builds.iter().collect();
    let groups = groups_of(&refs);
    let release = super::totest::latest_release(c, project_id)?;
    let newest_group = groups.first();
    let newest_short = newest_group.map(|g| primary(g).short_id());
    // An item of any record of a build (one commit, several platforms) counts as the build's,
    // which goes by its primary record: in the menu, the filter, the table and the chips.
    let primary_of: HashMap<String, String> = groups
        .iter()
        .flat_map(|g| {
            let p = primary(g).short_id();
            g.iter().map(move |r| (r.short_id(), p.clone()))
        })
        .collect();
    let mut items = records::query(
        c,
        &Query { project_id: Some(project_id), kinds: vec![Kind::Test], limit: 0, ..Default::default() },
    )?;
    items.sort_by_key(|r| (r.created_at, r.number));
    let cx = Context {
        builds: &refs,
        newest: newest_short.as_deref(),
        release: release.as_deref(),
        release_page: false,
        primary: Some(&primary_of),
        now,
    };
    let build_of = |r: &Record| crate::totest::build_of(r, &refs).map(|b| primary_of.get(&b).cloned().unwrap_or(b));
    let in_newest = |r: &Record| newest_short.is_some() && build_of(r) == newest_short;

    let newest = newest_group.map(|g| {
        let p = primary(g);
        let mut platforms: Vec<Platform> = g.iter().map(platform).collect();
        platforms.sort_by_key(|x| (x.os != if windows_first { "windows" } else { "linux" }, x.name.clone()));
        Newest {
            short: p.short_id(),
            url: p.url(),
            title: p.title.clone(),
            name: p.str("name").to_owned(),
            when: ago(p.created_at, now),
            // Agents write as their project.
            by: if p.author.is_empty() || p.author == p.project || p.author == "agent" {
                "the agent".into()
            } else {
                p.author.clone()
            },
            platforms,
            needs: p
                .data
                .get("needs")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default(),
            run: g
                .iter()
                .find(|r| platform_of(r) == crate::builds::DEFAULT_PLATFORM)
                .map(|r| format!("flow run {} {}", r.str("product"), r.number))
                .unwrap_or_default(),
        }
    });
    let new_all: Vec<&Record> = items.iter().filter(|r| in_newest(r)).collect();
    let new_total = new_all.len();
    let new_tried = new_all
        .iter()
        .filter(|r| {
            r.state != "open" || r.data.get("problems").and_then(Value::as_array).is_some_and(|p| !p.is_empty())
        })
        .count();
    let new_items: Vec<Item> = new_all.iter().filter(|r| r.state == "open").map(|r| item(r, &cx)).collect();

    // The filters, each menu counting what its choices would show with the others as they are.
    let words: Vec<String> = f.q.to_lowercase().split_whitespace().map(str::to_owned).collect();
    let search_ok = |r: &Record| {
        let hay = format!("{} {} {} {}", r.short_id(), r.title, r.body, r.str("category")).to_lowercase();
        words.iter().all(|w| hay.contains(w.as_str()))
    };
    let cat_ok = |r: &Record| f.cats.is_empty() || f.cats.iter().any(|c| c == r.str("category"));
    let from_ok = |r: &Record| match f.from.as_str() {
        "" => true,
        "none" => build_of(r).is_none(),
        b => build_of(r).as_deref() == Some(b),
    };
    // Unfiltered, the list leaves out what the newest build's card shows.
    let filtering = f.filtering();
    let listed = |r: &Record| filtering || !in_newest(r);
    let mut cat_n: Vec<(String, usize)> = Vec::new();
    for r in items.iter().filter(|r| listed(r) && f.state_ok(r) && from_ok(r) && search_ok(r)) {
        let cat = r.str("category").to_owned();
        match cat_n.iter_mut().find(|x| x.0 == cat) {
            Some(x) => x.1 += 1,
            None => cat_n.push((cat, 1)),
        }
    }
    for c in &f.cats {
        if !cat_n.iter().any(|x| &x.0 == c) {
            cat_n.push((c.clone(), 0));
        }
    }
    let cats: Vec<Choice> = cat_n
        .into_iter()
        .map(|(cat, n)| {
            let on = f.cats.contains(&cat);
            let next = f.with(|x| if on { x.cats.retain(|c| c != &cat) } else { x.cats.push(cat.clone()) });
            (cat, n.to_string(), next.href(slug), on)
        })
        .collect();
    let mut froms: Vec<Choice> =
        vec![("Any build".into(), String::new(), f.with(|x| x.from.clear()).href(slug), f.from.is_empty())];
    for g in &groups {
        let p = primary(g);
        let short = p.short_id();
        let n = items
            .iter()
            .filter(|r| {
                listed(r)
                    && f.state_ok(r)
                    && cat_ok(r)
                    && search_ok(r)
                    && build_of(r).as_deref() == Some(short.as_str())
            })
            .count();
        if n == 0 && f.from != short {
            continue;
        }
        let label = format!("{short} · {}", tz.day(p.created_at));
        froms.push((label, n.to_string(), f.with(|x| x.from = short.clone()).href(slug), f.from == short));
    }
    let before = items
        .iter()
        .filter(|r| listed(r) && f.state_ok(r) && cat_ok(r) && search_ok(r) && build_of(r).is_none())
        .count();
    if before > 0 || f.from == "none" {
        froms.push((
            "Before any build".into(),
            before.to_string(),
            f.with(|x| x.from = "none".into()).href(slug),
            f.from == "none",
        ));
    }
    let states = [
        ("", "Open"),
        ("problem", "With a problem"),
        ("tested", "Works"),
        ("skipped", "Skipped or dropped"),
        ("all", "All"),
    ]
    .into_iter()
    .map(|(k, label)| {
        let g = f.with(|x| x.state = k.into());
        let n = items
            .iter()
            .filter(|r| (g.filtering() || !in_newest(r)) && g.state_ok(r) && cat_ok(r) && from_ok(r) && search_ok(r))
            .count();
        (label.to_owned(), n.to_string(), g.href(slug), f.state == k)
    })
    .collect();

    let shown: Vec<Item> = items
        .iter()
        .filter(|r| listed(r) && f.state_ok(r) && cat_ok(r) && from_ok(r) && search_ok(r))
        .map(|r| item(r, &cx))
        .collect();
    let shown_n = shown.len();
    let groups_shown = grouped(shown);
    let (list_title, list_sub) = if filtering {
        ("Found".to_owned(), format!("{shown_n} by category"))
    } else {
        (
            "Still to try".to_owned(),
            match newest_short {
                Some(_) => format!("{shown_n} from earlier builds and releases, by category"),
                None => format!("{shown_n} by category"),
            },
        )
    };

    // Earlier builds: what you said about each, from its items and the older feedback form.
    let mut earlier = Vec::new();
    for g in groups.iter().skip(1) {
        let p = primary(g);
        let ids: Vec<String> = g.iter().map(|r| r.short_id()).collect();
        let of_build: Vec<&Record> =
            items.iter().filter(|r| crate::totest::build_of(r, &refs).is_some_and(|b| ids.contains(&b))).collect();
        let (mut works, mut problems, mut skipped) = (0, 0, 0);
        for r in &of_build {
            if r.state == "tested" {
                works += 1;
            } else if r.data.get("problems").and_then(Value::as_array).is_some_and(|p| !p.is_empty()) {
                problems += 1;
            } else if r.str("dropped_because") == "skipped by the owner" {
                skipped += 1;
            }
        }
        for b in g {
            let mut stmt = c.prepare(
                "SELECT r.state FROM links l JOIN records r ON r.id = l.from_id WHERE l.to_id = ?1 AND l.relation = 'feedback_on'",
            )?;
            let states: Vec<String> = stmt.query_map([b.id], |x| x.get(0))?.collect::<Result<_, _>>()?;
            for s in states {
                match s.as_str() {
                    "works" => works += 1,
                    "problem" => problems += 1,
                    "skipped" => skipped += 1,
                    _ => {}
                }
            }
        }
        let mut feedback = Vec::new();
        if works > 0 {
            feedback.push(("ok", format!("{works} work{}", if works == 1 { "s" } else { "" })));
        }
        if problems > 0 {
            feedback.push(("bad", format!("{problems} problem{}", if problems == 1 { "" } else { "s" })));
        }
        if skipped > 0 {
            feedback.push(("", format!("{skipped} skipped")));
        }
        let mut names: Vec<String> =
            g.iter().map(|r| platform_of(r).split('-').next().unwrap_or("").to_owned()).collect();
        names.sort();
        names.dedup();
        earlier.push(Earlier {
            short: p.short_id(),
            url: p.url(),
            number: p.number,
            title: p.title.clone(),
            name: p.str("name").to_owned(),
            platforms: names.join(" · "),
            published: if tz.day_start(p.created_at) == tz.day_start(now) {
                format!("today {}", tz.hm(p.created_at))
            } else {
                tz.short(p.created_at)
            },
            published_at: p.created_at,
            feedback_n: works + problems + skipped,
            feedback,
            run: if platform_of(p) == crate::builds::DEFAULT_PLATFORM {
                format!("flow run {} {}", p.str("product"), p.number)
            } else {
                String::new()
            },
            download: format!("{}/download", p.url()),
        });
    }
    let earlier_total = earlier.len();
    const SHOWN: usize = 6;
    let earlier_all =
        (!f.all_builds && earlier_total > SHOWN).then(|| f.with(|x| x.all_builds = true).href(slug) + "#earlier");
    if !f.all_builds {
        earlier.truncate(SHOWN);
    }

    let mut keep = Vec::new();
    for (k, v) in [("tcat", f.cats.join("|")), ("tfrom", f.from.clone()), ("tstate", f.state.clone())] {
        if !v.is_empty() {
            keep.push((k, v));
        }
    }
    if f.all_builds {
        keep.push(("ball", "1".into()));
    }
    let picked = |v: &Vec<Choice>| v.iter().find(|c| c.3).map(|c| c.0.clone());
    Ok(BuildsView {
        none: builds.is_empty() && items.is_empty(),
        newest,
        new_items,
        new_total,
        new_tried,
        back: f.href(slug),
        is_owner,
        q: f.q.clone(),
        keep,
        cat_label: match f.cats.len() {
            0 => "All".into(),
            1 | 2 => f.cats.join(", "),
            n => format!("{n} picked"),
        },
        cats,
        cats_clear: (!f.cats.is_empty()).then(|| f.with(|x| x.cats.clear()).href(slug)),
        from_label: if f.from.is_empty() {
            "Any build".into()
        } else if f.from == "none" {
            "Before any build".into()
        } else {
            f.from.clone()
        },
        froms,
        state_label: picked(&states).unwrap_or_else(|| "Open".into()),
        states,
        reset: filtering.then(|| {
            f.with(|x| {
                x.q.clear();
                x.cats.clear();
                x.from.clear();
                x.state.clear();
            })
            .href(slug)
        }),
        list_title,
        list_sub,
        groups: groups_shown,
        filtering,
        earlier,
        earlier_total,
        earlier_all,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::NewRecord;
    use serde_json::json;

    #[test]
    fn an_earlier_builds_items_count_whichever_platform_they_came_with() {
        let db = crate::db::Db::open_memory();
        db.call_blocking(|c| {
            let p = crate::projects::create(c, "vinculum", "Vinculum", "test")?.id;
            let add = |kind, title: &str, at: i64, data: Value| {
                let data = data.as_object().cloned().unwrap_or_default();
                let new = NewRecord {
                    kind: Some(kind),
                    title: title.into(),
                    created_at: Some(at),
                    data,
                    ..Default::default()
                };
                records::create(c, p, "vinculum", new)
            };
            add(Kind::Test, "Tray icon", 50, json!({"category": "Tray"}))?;
            // One commit for two platforms, Windows first (B-1, B-2), then a newer build (B-3).
            add(Kind::Build, "One", 100, json!({"commit": "c1", "platform": "windows-x86_64"}))?;
            add(Kind::Build, "One", 101, json!({"commit": "c1"}))?;
            add(Kind::Build, "Two", 200, json!({"commit": "c2"}))?;
            let v = view(c, "vinculum", p, &Filter::default(), true, false, 300, &Tz::default())?;
            let from: Vec<(&str, &str)> = v.froms.iter().map(|x| (x.0.as_str(), x.1.as_str())).collect();
            assert!(from.iter().any(|(label, n)| label.starts_with("B-2 ") && *n == "1"), "{from:?}");
            let item = &v.groups[0].items[0];
            assert_eq!(item.chip.as_ref().map(|x| x.1.as_str()), Some("in B-2"));
            let picked = Filter::new(None, None, Some("B-2"), None, false);
            let v = view(c, "vinculum", p, &picked, true, false, 300, &Tz::default())?;
            assert_eq!(v.groups.iter().map(|g| g.items.len()).sum::<usize>(), 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn filters_round_trip_through_the_address() {
        let f = Filter::new(Some("hover"), Some("Agents|Paint"), Some("B-66"), Some("problem"), true);
        assert_eq!(f.href("x"), "/p/x?tab=builds&tq=hover&tcat=Agents%7CPaint&tfrom=B-66&tstate=problem&ball=1");
        assert_eq!(Filter::new(None, None, Some("../x"), Some("open"), false), Filter::default());
        assert_eq!(Filter::new(None, None, Some("B-"), None, false).from, "");
        assert_eq!(Filter::new(None, None, Some("none"), None, false).from, "none");
    }
}
