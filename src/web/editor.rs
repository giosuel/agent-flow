//! The Markdown editor the project document and the handbook share (the owner, 2026-11-18: "make
//! the handbook the same as the project document … in the markdown editor instead with the
//! preview. reuse the components"): the whole file as Markdown, a live preview beside it (Markdown
//! or Preview on the phone), Ctrl+S, closing asks before dropping edits, a banner when a newer
//! version was saved meanwhile, and the other side's version of a section both changed, to pick.
//! The template is `md_editor.html`; the script and styles go by `form[data-md-editor]`.

/// A section both changed, offered to pick: its title, the other side's version (Markdown, for the
/// editor, and rendered), and whether the editor's text still has it.
pub struct Conflict {
    pub title: String,
    pub theirs: String,
    pub theirs_html: String,
    pub mine_kept: bool,
}

pub struct MdEditor {
    /// Where Save posts.
    pub action: String,
    /// Fields the save needs besides `base_rev` and `text` (e.g. the file's name).
    pub hidden: Vec<(&'static str, String)>,
    /// The revision the text started from.
    pub base_rev: i64,
    pub text: String,
    pub preview_html: String,
    /// Posts `text` and answers the preview's HTML.
    pub preview_url: String,
    /// Answers `{rev, author, changed}` for `since=<rev>`, for the banner; empty: no banner.
    pub rev_url: String,
    pub conflicts: Vec<Conflict>,
    /// Who saved the version merged in, for the conflict banner.
    pub other: String,
    pub cancel_href: String,
    /// Cancel stays in the page's dialog.
    pub cancel_dlg: bool,
    /// What the text box holds, for screen readers.
    pub label: String,
    /// The footer's line on the right, e.g. who hears of the edit.
    pub note: String,
}

impl MdEditor {
    /// The conflicts of a merge, ready to show.
    pub fn conflicts_of(list: Vec<crate::projectdoc::Conflict>) -> Vec<Conflict> {
        list.into_iter()
            .map(|c| {
                let body = c.theirs.split_once('\n').map(|x| x.1).unwrap_or("");
                Conflict {
                    theirs_html: crate::markdown::to_html(body),
                    title: c.title,
                    theirs: c.theirs.trim_end().to_owned(),
                    mine_kept: c.mine_kept,
                }
            })
            .collect()
    }
}
