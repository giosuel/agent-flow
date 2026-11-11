//! Small charts drawn on the server: stacked bars, stacked areas and lines. The marks are an SVG
//! that stretches to its box (so it fills any width); the labels are HTML around it, so they keep
//! their size on the phone. Every mark has a title for hover, and the pages show the same numbers
//! in a table.

use std::fmt::Write;

/// Project colours, validated for colour blindness on the dark surface (2026-11-10, the dataviz
/// validator): fixed by project, never by rank, so a filter doesn't repaint anyone.
const KNOWN: &[(&str, &str)] = &[
    ("imaginarium", "#3987e5"),
    ("vinculum", "#d95926"),
    ("cold-trace", "#199e70"),
    ("antimony", "#9085e9"),
    ("realer-xpad", "#d55181"),
];
pub const OTHER: &str = "#6E6E7A";

/// A project's colour: its own, or a free one of the five in project order, or grey.
pub fn colors(slugs: &[String]) -> Vec<&'static str> {
    let mut free: Vec<&'static str> =
        KNOWN.iter().filter(|(s, _)| !slugs.iter().any(|x| x == s)).map(|(_, c)| *c).collect();
    free.reverse();
    slugs
        .iter()
        .map(|s| KNOWN.iter().find(|(k, _)| k == s).map(|(_, c)| *c).or_else(|| free.pop()).unwrap_or(OTHER))
        .collect()
}

pub struct Series {
    pub name: String,
    pub color: &'static str,
    pub values: Vec<f64>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

const W: f64 = 1000.0;
const H: f64 = 200.0;

fn open_svg(out: &mut String, label: &str) {
    let _ = write!(
        out,
        r#"<svg class="chart-marks" viewBox="0 0 {W} {H}" preserveAspectRatio="none" role="img" aria-label="{}">"#,
        esc(label)
    );
    // Three recessive grid lines.
    for i in 1..4 {
        let y = H * i as f64 / 4.0;
        let _ =
            write!(out, r#"<line class="grid" x1="0" x2="{W}" y1="{y}" y2="{y}" vector-effect="non-scaling-stroke"/>"#);
    }
}

/// Stacked bars, one per label. `href(i)` makes a bar a link (a day opens its hours).
pub fn stacked_bars(
    label: &str,
    labels: &[String],
    series: &[Series],
    fmt: &dyn Fn(f64) -> String,
    href: Option<&dyn Fn(usize) -> String>,
) -> (String, f64) {
    let n = labels.len().max(1);
    let totals: Vec<f64> =
        (0..n).map(|i| series.iter().map(|s| s.values.get(i).copied().unwrap_or(0.0)).sum()).collect();
    let max = nice_max(totals.iter().copied().fold(0.0, f64::max));
    let mut out = String::new();
    open_svg(&mut out, label);
    let slot = W / n as f64;
    let bar = (slot * 0.7).max(1.0);
    for (i, day) in labels.iter().enumerate() {
        let x = i as f64 * slot + (slot - bar) / 2.0;
        if let Some(h) = href {
            let _ = write!(out, r#"<a href="{}">"#, esc(&h(i)));
            // The whole column is the target, not just the bar.
            let _ = write!(
                out,
                r#"<rect class="hit" x="{}" y="0" width="{slot}" height="{H}"><title>{} · {}</title></rect>"#,
                i as f64 * slot,
                esc(day),
                esc(&fmt(totals[i]))
            );
        }
        let mut y = H;
        for s in series {
            let v = s.values.get(i).copied().unwrap_or(0.0);
            if v <= 0.0 || max <= 0.0 {
                continue;
            }
            let h = v / max * H;
            y -= h;
            // A thin gap between stacked segments, in the surface colour.
            let _ = write!(
                out,
                r#"<rect x="{x:.1}" y="{y:.2}" width="{bar:.1}" height="{:.2}" fill="{}" class="seg"><title>{} · {} · {}</title></rect>"#,
                (h - 0.8).max(0.4),
                s.color,
                esc(day),
                esc(&s.name),
                esc(&fmt(v))
            );
        }
        if href.is_some() {
            out.push_str("</a>");
        }
    }
    out.push_str("</svg>");
    (out, max)
}

/// Stacked areas over evenly spaced points.
pub fn stacked_areas(label: &str, points: usize, series: &[Series], limit: Option<f64>) -> (String, f64) {
    let n = points.max(2);
    let totals: Vec<f64> =
        (0..n).map(|i| series.iter().map(|s| s.values.get(i).copied().unwrap_or(0.0)).sum()).collect();
    let max = nice_max(totals.iter().copied().fold(limit.unwrap_or(0.0), f64::max));
    let mut out = String::new();
    open_svg(&mut out, label);
    let x = |i: usize| i as f64 / (n - 1) as f64 * W;
    let y = |v: f64| if max > 0.0 { H - v / max * H } else { H };
    let mut base = vec![0.0; n];
    for s in series {
        let top: Vec<f64> = (0..n).map(|i| base[i] + s.values.get(i).copied().unwrap_or(0.0)).collect();
        let mut d = String::new();
        for (i, t) in top.iter().enumerate() {
            let _ = write!(d, "{}{:.1},{:.2} ", if i == 0 { "M" } else { "L" }, x(i), y(*t));
        }
        for i in (0..n).rev() {
            let _ = write!(d, "L{:.1},{:.2} ", x(i), y(base[i]));
        }
        let _ = write!(
            out,
            r#"<path d="{d}Z" fill="{}" fill-opacity="0.85"><title>{}</title></path>"#,
            s.color,
            esc(&s.name)
        );
        base = top;
    }
    if let Some(l) = limit {
        let _ = write!(
            out,
            r#"<line class="limit" x1="0" x2="{W}" y1="{0:.2}" y2="{0:.2}" vector-effect="non-scaling-stroke"><title>machine limit</title></line>"#,
            y(l)
        );
    }
    out.push_str("</svg>");
    (out, max)
}

/// One line per series (2px, never filled), a point in the middle of each slot, so they sit
/// under the same labels as bars.
pub fn lines(label: &str, points: usize, series: &[Series]) -> (String, f64) {
    let n = points.max(2);
    let max = nice_max(series.iter().flat_map(|s| s.values.iter().copied()).fold(0.0, f64::max));
    let mut out = String::new();
    open_svg(&mut out, label);
    for s in series {
        let mut d = String::new();
        for i in 0..n {
            let v = s.values.get(i).copied().unwrap_or(0.0);
            let yy = if max > 0.0 { H - v / max * H } else { H };
            let _ = write!(d, "{}{:.1},{:.2} ", if i == 0 { "M" } else { "L" }, (i as f64 + 0.5) / n as f64 * W, yy);
        }
        let _ = write!(
            out,
            r#"<path d="{d}" fill="none" stroke="{}" stroke-width="2" vector-effect="non-scaling-stroke" stroke-linejoin="round"><title>{}</title></path>"#,
            s.color,
            esc(&s.name)
        );
    }
    out.push_str("</svg>");
    (out, max)
}

/// A round number at or above `v` for the top of the scale: 1, 2, 2.5 or 5 × a power of ten.
pub fn nice_max(v: f64) -> f64 {
    if v <= 0.0 {
        return 0.0;
    }
    let p = 10f64.powf(v.log10().floor());
    [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|m| m * p).find(|x| *x >= v).unwrap_or(10.0 * p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_follow_the_project() {
        let c = colors(&["vinculum".into(), "giosuelio".into(), "imaginarium".into()]);
        assert_eq!(c, vec!["#d95926", "#199e70", "#3987e5"], "a new project takes a free colour");
        assert_eq!(nice_max(7.3), 10.0);
        assert_eq!(nice_max(23.0), 25.0);
        assert_eq!(nice_max(1_300_000.0), 2_000_000.0);
    }

    #[test]
    fn bars_escape_and_link() {
        let s = vec![Series { name: "<b>".into(), color: "#3987e5", values: vec![1.0, 3.0] }];
        let labels = vec!["Mon".into(), "Tue".into()];
        let link = |i: usize| format!("?day={i}");
        let (svg, max) = stacked_bars("t", &labels, &s, &|v| format!("{v}"), Some(&link));
        assert_eq!(max, 5.0);
        assert!(svg.contains("&lt;b&gt;") && !svg.contains("<b>"));
        assert!(svg.contains(r#"href="?day=1""#));
    }
}
