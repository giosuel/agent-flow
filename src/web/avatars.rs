//! The 15 profile icons people pick from (the owner, 2026-11-25, D-270: "a set of 15 icons that the
//! user can pick from. it should be in the project's style"; N-24). Files in static/avatars/; the
//! agents keep the robot.

pub const AVATARS: &[&str] = &[
    "fox",
    "owl",
    "cat",
    "bear",
    "frog",
    "rocket",
    "whale",
    "moth",
    "ghost",
    "penguin",
    "rabbit",
    "axolotl",
    "mushroom",
    "planet",
    "jellyfish",
];

pub fn is_avatar(name: &str) -> bool {
    AVATARS.contains(&name)
}

/// Whether `a` is the icon `chosen` (for the picker's checked one).
pub fn same(a: &str, chosen: &str) -> bool {
    a == chosen
}

/// A person's icon as HTML (ui::avatar uses it too): `size` is "", "sm" or "lg"; no icon yet, the
/// person glyph.
pub fn html(icon: &str, size: &str) -> String {
    if is_avatar(icon) {
        format!(r#"<img class="avatar {size}" src="/static/avatars/{icon}.svg" alt="" aria-hidden="true">"#)
    } else {
        format!(r#"<span class="avatar none {size}" aria-hidden="true">{}</span>"#, super::icons::icon("person"))
    }
}
