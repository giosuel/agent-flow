//! Screenshots agents upload for the owner (the owner, 2026-11-11: "agents should be able to upload
//! any screenshots with a name, optional description and category (e.g. test build, mockup, spike
//! test)"). Each is a record (S-4) whose title is the name, body the description, and data the
//! category and file; the image is in `<data_dir>/screenshots/<project>/S-4.<ext>`.

use std::path::{Path, PathBuf};

use anyhow::bail;

/// The largest screenshot accepted.
pub const MAX_BYTES: usize = 10 * 1024 * 1024;

/// What a file is, from its first bytes (never from what the uploader says): PNG, JPEG or WebP.
/// SVG and anything else are refused, since an image served from our origin must not run code.
pub fn sniff(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "png"))
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("image/jpeg", "jpg"))
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(("image/webp", "webp"))
    } else {
        None
    }
}

/// Width and height, when the format says them up front (PNG; the others are shown as they come).
pub fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((w, h))
}

/// "Test build " → "test build": categories are free text, kept short and in lower case so the
/// same one is never two filters.
pub fn category(raw: &str) -> anyhow::Result<String> {
    let c = raw.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    if c.is_empty() {
        bail!("give it a category, e.g. test build, mockup, spike test or bug");
    }
    if c.chars().count() > 40 || c.chars().any(char::is_control) {
        bail!("a category is a few words (up to 40 characters)");
    }
    Ok(c)
}

/// A mockup's state (the owner, 2026-11-12: "mockups will be visible as screenshots in agent flow.
/// it might be good to put their claude artifact URL in the description, as well as the state").
pub const MOCKUP_STATES: &[&str] = &["proposed", "approved", "built"];

pub fn check_mockup_state(s: &str) -> anyhow::Result<()> {
    if !MOCKUP_STATES.contains(&s) {
        bail!("a mockup is proposed, approved or built");
    }
    Ok(())
}

/// A link to the design (a claude.ai artifact, usually): https only.
pub fn check_design_url(u: &str) -> anyhow::Result<()> {
    if !u.starts_with("https://") || u.len() > 500 || u.chars().any(|c| c.is_whitespace() || c == '"' || c == '<') {
        bail!("the design link is an https:// address");
    }
    Ok(())
}

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("screenshots")
}

/// Where a screenshot's image is kept. Slugs, record numbers and extensions are all checked
/// values, so the path stays inside the folder.
pub fn path(data_dir: &Path, slug: &str, number: i64, ext: &str) -> PathBuf {
    dir(data_dir).join(slug).join(format!("S-{number}.{ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_categories() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&1280u32.to_be_bytes());
        png.extend_from_slice(&800u32.to_be_bytes());
        assert_eq!(sniff(&png), Some(("image/png", "png")));
        assert_eq!(png_size(&png), Some((1280, 800)));
        assert_eq!(sniff(b"\xFF\xD8\xFF\xE0rest"), Some(("image/jpeg", "jpg")));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(("image/webp", "webp")));
        assert_eq!(sniff(b"<svg xmlns='http://www.w3.org/2000/svg'><script/></svg>"), None);
        assert_eq!(category("  Test   Build ").unwrap(), "test build");
        assert!(category(" ").is_err() && category(&"x".repeat(41)).is_err());
    }
}
