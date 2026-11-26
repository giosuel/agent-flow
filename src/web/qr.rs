//! A link's QR code as SVG (Settings › Passkeys › Sign in on another device, D-274): scanned with
//! the new device's camera, it opens the link there.

use qrcodegen::{QrCode, QrCodeEcc};

/// The code for `text`, dark on light with a quiet zone; empty if it doesn't fit a QR code.
pub fn svg(text: &str) -> String {
    let Ok(qr) = QrCode::encode_text(text, QrCodeEcc::Medium) else { return String::new() };
    let (n, quiet) = (qr.size(), 3);
    let dim = n + 2 * quiet;
    let mut path = String::new();
    for y in 0..n {
        for x in 0..n {
            if qr.get_module(x, y) {
                path.push_str(&format!("M{},{}h1v1h-1z", x + quiet, y + quiet));
            }
        }
    }
    format!(
        r#"<svg class="qr" viewBox="0 0 {dim} {dim}" role="img" aria-label="QR code of the link" shape-rendering="crispEdges"><rect width="{dim}" height="{dim}" fill="white"/><path d="{path}" fill="black"/></svg>"#
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_link_becomes_a_code() {
        let svg = super::svg("https://flow.giosuel.io/device/af_lnk_0123456789abcdef0123456789abcdef");
        assert!(svg.starts_with("<svg class=\"qr\" viewBox=\"0 0 ") && svg.contains("M3,3h1v1h-1z"), "{svg}");
    }
}
