//! Small line icons (24×24, currentColor), inlined into pages.

pub fn icon(name: &str) -> &'static str {
    match name {
        "cpu" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="6" y="6" width="12" height="12" rx="2"/><rect x="9.5" y="9.5" width="5" height="5" rx="1"/><path d="M9 2v4M15 2v4M9 18v4M15 18v4M2 9h4M2 15h4M18 9h4M18 15h4"/></svg>"#
        }
        "memory" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="2" y="7" width="20" height="10" rx="2"/><path d="M6 7v10M10 7v10M14 7v10M18 7v10M5 17v3M19 17v3"/></svg>"#
        }
        "disk" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="4" width="18" height="16" rx="3"/><circle cx="12" cy="11" r="4"/><circle cx="12" cy="11" r="1"/><path d="M7 17h2"/></svg>"#
        }
        "tmp" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M13 2 4 14h7l-1 8 9-12h-7z"/></svg>"#
        }
        "load" => r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M3 12h4l3-8 4 16 3-8h4"/></svg>"#,
        "draft" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M4 20h4L19 9l-4-4L4 16z"/><path d="m13.5 6.5 4 4"/></svg>"#
        }
        "pr" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><circle cx="6" cy="6" r="2.5"/><circle cx="6" cy="18" r="2.5"/><circle cx="18" cy="18" r="2.5"/><path d="M6 8.5v7M18 15.5V9a3 3 0 0 0-3-3h-4m2-2-2 2 2 2"/></svg>"#
        }
        "issue" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="1.5"/></svg>"#
        }
        "session" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="m7 9 3 3-3 3M12 15h5"/></svg>"#
        }
        "out" => r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M8 16 16 8M10 8h6v6"/></svg>"#,
        "inbox" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M3 13h5l2 3h4l2-3h5"/><path d="M5 5h14l2 8v6H3v-6z"/></svg>"#
        }
        "info" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 11v6M12 7.5v.5"/></svg>"#
        }
        "stack" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M12 3 3 8l9 5 9-5-9-5z"/><path d="m3 13 9 5 9-5"/></svg>"#
        }
        "platform" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="4" width="18" height="12" rx="2"/><path d="M8 20h8M12 16v4"/></svg>"#
        }
        "model" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M12 3v3M12 18v3M3 12h3M18 12h3M6 6l2 2M16 16l2 2M6 18l2-2M16 8l2-2"/><circle cx="12" cy="12" r="3"/></svg>"#
        }
        "rules" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M9 6h11M9 12h11M9 18h11"/><path d="m3.5 6 1 1 2-2M3.5 12l1 1 2-2M3.5 18l1 1 2-2"/></svg>"#
        }
        "git" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><circle cx="6" cy="6" r="2.5"/><circle cx="6" cy="18" r="2.5"/><circle cx="18" cy="12" r="2.5"/><path d="M6 8.5v7M8 7.5c4 1 6 2 8 3"/></svg>"#
        }
        "folder" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>"#
        }
        "machine" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="4" width="18" height="7" rx="1.5"/><rect x="3" y="13" width="18" height="7" rx="1.5"/><path d="M7 7.5h.01M7 16.5h.01"/></svg>"#
        }
        "port" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M8 3v5M16 3v5M5 8h14v4a7 7 0 0 1-14 0zM12 19v2"/></svg>"#
        }
        "question" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M21 12a8 8 0 0 1-11.6 7.1L4 20l1-4.6A8 8 0 1 1 21 12z"/></svg>"#
        }
        "decision" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg>"#
        }
        "task" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="4" y="4" width="16" height="16" rx="3"/><path d="m8.5 12 2.5 2.5 4.5-5"/></svg>"#
        }
        "doc" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M6 3h9l4 4v14H6z"/><path d="M14 3v5h5M9 13h7M9 17h5"/></svg>"#
        }
        "merge" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><circle cx="6" cy="6" r="2.5"/><circle cx="6" cy="18" r="2.5"/><circle cx="18" cy="12" r="2.5"/><path d="M6 8.5v7M8 7.5c4 1 6 2 8 3"/></svg>"#
        }
        "tag" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M3 12V4h8l10 10-8 8L3 12z"/><circle cx="7.5" cy="8.5" r="1.5"/></svg>"#
        }
        "build" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M21 8 12 3 3 8v8l9 5 9-5z"/><path d="m3 8 9 5 9-5M12 13v8"/></svg>"#
        }
        "bell" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M6 16V11a6 6 0 0 1 12 0v5l2 2H4z"/><path d="M10 21h4"/></svg>"#
        }
        "key" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><circle cx="7.5" cy="15.5" r="3.5"/><path d="m10 13 9-9M16 7l2.5 2.5M14 9l2 2"/></svg>"#
        }
        "laptop" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="4.5" y="5" width="15" height="10" rx="1.5"/><path d="M2.5 19h19"/></svg>"#
        }
        "window" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="4.5" width="18" height="15" rx="2"/><path d="M3 9h18"/></svg>"#
        }
        "robot" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="4" y="8" width="16" height="11" rx="2.5"/><path d="M12 8V4.5M9 13h.01M15 13h.01M9.5 16.5h5"/></svg>"#
        }
        "server" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="3.5" y="4" width="17" height="7" rx="1.5"/><rect x="3.5" y="13" width="17" height="7" rx="1.5"/><path d="M7 7.5h.01M7 16.5h.01"/></svg>"#
        }
        "code" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="m8 7-5 5 5 5M16 7l5 5-5 5"/></svg>"#
        }
        "person" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="8" r="3.5"/><path d="M5 20a7 7 0 0 1 14 0"/></svg>"#
        }
        "hammer" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="m14 6 4 4M3 21l9-9M12.5 4.5l7 7-2.5 2.5-7-7z"/></svg>"#
        }
        "pause" => r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M8 5v14M16 5v14"/></svg>"#,
        "play" => r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M7 5v14l11-7z"/></svg>"#,
        "search" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><circle cx="11" cy="11" r="6.5"/><path d="m16 16 4.5 4.5"/></svg>"#
        }
        "filter" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M4 6h16M7 12h10M10 18h4"/></svg>"#
        }
        "copy" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V5a1 1 0 0 0-1-1H5a1 1 0 0 0-1 1v10a1 1 0 0 0 1 1h3"/></svg>"#
        }
        "message" => r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M4 5h16v11H8l-4 4z"/></svg>"#,
        "grid" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="3.5" y="3.5" width="7" height="7" rx="1.5"/><rect x="13.5" y="3.5" width="7" height="7" rx="1.5"/><rect x="3.5" y="13.5" width="7" height="7" rx="1.5"/><rect x="13.5" y="13.5" width="7" height="7" rx="1.5"/></svg>"#
        }
        "chart" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M4 20h16"/><path d="M7 16v-5M12 16V6M17 16v-8"/></svg>"#
        }
        "book" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M4 5.5A2.5 2.5 0 0 1 6.5 3H20v15H6.5A2.5 2.5 0 0 0 4 20.5z"/><path d="M4 20.5A2.5 2.5 0 0 0 6.5 23H20v-5"/></svg>"#
        }
        "more" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><circle cx="5" cy="12" r="1.3"/><circle cx="12" cy="12" r="1.3"/><circle cx="19" cy="12" r="1.3"/></svg>"#
        }
        "list" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M9 6h11M9 12h11M9 18h11M4.5 6h.01M4.5 12h.01M4.5 18h.01"/></svg>"#
        }
        "gear" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"/></svg>"#
        }
        "pencil" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M4 20h4L19 9l-4-4L4 16z"/><path d="m13.5 6.5 4 4"/></svg>"#
        }
        "back" => r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="m15 5-7 7 7 7"/></svg>"#,
        "plus" => r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M12 5v14M5 12h14"/></svg>"#,
        "chevron" => r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="m9 6 6 6-6 6"/></svg>"#,
        // Notes by file type (the owner, 2026-11-18: "lets make the icon dependent on the file type").
        "ftext" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M6 3h9l4 4v14H6z"/><path d="M14 3v5h5M9 12h7M9 15h7M9 18h4"/></svg>"#
        }
        "note" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M4 4h16v11l-5 5H4z"/><path d="M15 20v-5h5M8 9h8M8 13h5"/></svg>"#
        }
        "archive" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="4" width="18" height="5" rx="1"/><path d="M5 9v10a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V9M10 13h4"/></svg>"#
        }
        "image" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="4" width="18" height="16" rx="2"/><circle cx="9" cy="10" r="1.6"/><path d="m4 18 5-5 4 4 3-3 4 4"/></svg>"#
        }
        "table" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="M3 10h18M3 15h18M9 4v16"/></svg>"#
        }
        "pdf" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M6 3h9l4 4v14H6z"/><path d="M14 3v5h5"/><path d="M9 17v-5h1.5a1.5 1.5 0 0 1 0 3H9"/></svg>"#
        }
        "file" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M6 3h9l4 4v14H6z"/><path d="M14 3v5h5"/></svg>"#
        }
        "download" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M12 4v11M7 10l5 5 5-5M5 20h14"/></svg>"#
        }
        "fork" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><circle cx="6" cy="5" r="2"/><circle cx="18" cy="5" r="2"/><circle cx="12" cy="19" r="2"/><path d="M6 7v1a4 4 0 0 0 4 4h4a4 4 0 0 0 4-4V7M12 12v5"/></svg>"#
        }
        "check" => r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="m5 12 5 5 9-10"/></svg>"#,
        "refresh" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M20 11a8 8 0 0 0-14.3-4.9L4 8"/><path d="M4 3v5h5"/><path d="M4 13a8 8 0 0 0 14.3 4.9L20 16"/><path d="M20 21v-5h-5"/></svg>"#
        }
        "history" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5M12 8v4l3 2"/></svg>"#
        }
        // Close, a caret, back and a warning: the library's (M15), instead of the "×", "‹" and "›"
        // glyphs dialogs drew in three sizes.
        "close" => r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6l12 12M18 6 6 18"/></svg>"#,
        "caret" => r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="m7 10 5 5 5-5"/></svg>"#,
        "prev" => r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="m15 6-6 6 6 6"/></svg>"#,
        "alert" => {
            r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="M12 4 2.8 20h18.4L12 4Z"/><path d="M12 10v4.5M12 17.5h.01"/></svg>"#
        }
        _ => r#"<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="3"/></svg>"#,
    }
}

/// Every icon, for the library's catalogue page (/ui).
pub const ALL: &[&str] = &[
    "cpu", "memory", "disk", "tmp", "load", "draft", "pr", "issue", "session", "out", "inbox", "info", "stack",
    "platform", "model", "rules", "git", "folder", "machine", "port", "question", "decision", "task", "doc", "merge",
    "tag", "build", "bell", "key", "laptop", "window", "robot", "server", "code", "person", "hammer", "pause", "play",
    "search", "filter", "copy", "message", "grid", "chart", "book", "more", "list", "gear", "pencil", "back", "plus",
    "chevron", "ftext", "note", "archive", "image", "table", "pdf", "file", "download", "fork", "check", "refresh",
    "history", "close", "caret", "prev", "alert",
];

/// The icons scripts draw (dialogs, the copy button, the bell's panel), as symbols on every page, so
/// static/*.js uses the same icons as the templates: `<svg class="i"><use href="#i-close"/></svg>`.
pub const SPRITE: &[&str] = &["close", "prev", "chevron", "copy", "check", "gear", "caret", "alert"];

pub fn sprite() -> String {
    let mut out =
        String::from(r#"<svg class="sprite" aria-hidden="true" width="0" height="0" style="position:absolute">"#);
    for name in SPRITE {
        let svg = icon(name);
        let inner = &svg[svg.find('>').map(|i| i + 1).unwrap_or(0)..svg.rfind("</svg>").unwrap_or(svg.len())];
        out.push_str(&format!(r#"<symbol id="i-{name}" viewBox="0 0 24 24">{inner}</symbol>"#));
    }
    out.push_str("</svg>");
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_sprite_holds_every_icon_scripts_use() {
        let s = super::sprite();
        for name in super::SPRITE {
            assert!(s.contains(&format!(r#"<symbol id="i-{name}" viewBox="0 0 24 24">"#)), "{name}");
            assert_ne!(super::icon(name), super::icon("no such icon"), "{name} is a real icon, not the fallback dot");
        }
        assert!(s.contains(r#"<path d="M6 6l12 12M18 6 6 18"/></symbol>"#), "{s}");
        for name in super::ALL {
            assert_ne!(super::icon(name), super::icon("no such icon"), "{name} in ALL has no icon");
        }
    }
}
