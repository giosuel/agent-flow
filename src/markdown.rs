//! Markdown to HTML for record bodies and guides. Agents write the text, so it's treated as
//! untrusted: raw HTML is shown as text, and links and images keep only http(s), mailto and
//! relative targets.

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};

pub fn to_html(md: &str) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events = Parser::new_ext(md, opts).map(|e| match e {
        Event::Html(s) | Event::InlineHtml(s) => Event::Text(s),
        Event::Start(Tag::Link { link_type, dest_url, title, id }) => {
            Event::Start(Tag::Link { link_type, dest_url: safe_url(dest_url), title, id })
        }
        Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
            Event::Start(Tag::Image { link_type, dest_url: safe_url(dest_url), title, id })
        }
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

fn safe_url(url: CowStr) -> CowStr {
    let lower = url.trim().to_ascii_lowercase();
    let scheme =
        lower.split_once(':').map(|(s, _)| s).filter(|s| !s.contains('/') && !s.contains('?') && !s.contains('#'));
    match scheme {
        None | Some("http") | Some("https") | Some("mailto") => url,
        Some(_) => CowStr::Borrowed("#"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_markdown_is_defused() {
        let html = to_html("Hi <script>alert(1)</script> [x](javascript:alert(1)) [y](https://a.b/c) [z](/p/v/T-1)");
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains(r##"href="#""##));
        assert!(html.contains(r#"href="https://a.b/c""#));
        assert!(html.contains(r#"href="/p/v/T-1""#));
        assert!(to_html("![i](data:text/html,x)").contains(r##"src="#""##));
        assert!(to_html("- a\n- b").contains("<li>a</li>"));
        assert!(to_html("```\ncode\n```").contains("<pre><code>"));
    }
}
