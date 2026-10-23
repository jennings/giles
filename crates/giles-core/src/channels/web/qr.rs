//! QR codes for the admin pages: a channel that needs linking (`NeedsAuth` with a QR payload, PLAN
//! section 8.5) shows its code as an inline SVG element, rendered here from the payload.

use std::fmt::Write as _;

use qrcode::types::{Color, QrError};
use qrcode::{EcLevel, QrCode};

/// The light margin around the code, in modules. Scanners need four (ISO/IEC 18004).
const QUIET_ZONE: usize = 4;

/// `payload` as a QR code: an `<svg>` element, dark modules on a white square with the quiet zone
/// (so it scans on a dark page too), sized by the stylesheet. The payload is only encoded, never
/// copied into the markup, so any payload is safe to render.
///
/// # Errors
///
/// [`QrError::DataTooLong`] if the payload doesn't fit in a QR code.
pub(super) fn svg(payload: &str) -> Result<String, QrError> {
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::M)?;
    let width = code.width();
    let size = width + 2 * QUIET_ZONE;
    let mut path = String::new();
    for (y, row) in code.to_colors().chunks(width).enumerate() {
        // One rectangle per horizontal run of dark modules.
        let mut x = 0;
        while x < width {
            if row[x] == Color::Light {
                x += 1;
                continue;
            }
            let start = x;
            while x < width && row[x] == Color::Dark {
                x += 1;
            }
            let run = x - start;
            let _ = write!(
                path,
                "M{} {}h{run}v1h-{run}z",
                start + QUIET_ZONE,
                y + QUIET_ZONE
            );
        }
    }
    Ok(format!(
        "<svg class=\"qr\" xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {size} {size}\" \
         shape-rendering=\"crispEdges\" role=\"img\" aria-label=\"QR code\">\
         <rect width=\"{size}\" height=\"{size}\" fill=\"#fff\"/>\
         <path fill=\"#000\" d=\"{path}\"/></svg>"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dark modules an [`svg`] draws, as a grid of `size` × `size` (quiet zone included).
    fn drawn(svg: &str) -> (usize, Vec<bool>) {
        let size: usize = svg
            .split("viewBox=\"0 0 ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .unwrap()
            .parse()
            .unwrap();
        let path = svg
            .split(" d=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let mut grid = vec![false; size * size];
        for rect in path.split('M').filter(|r| !r.is_empty()) {
            // `<x> <y>h<run>v1h-<run>z`
            let (x, rest) = rect.split_once(' ').unwrap();
            let (y, rest) = rest.split_once('h').unwrap();
            let run = rest.split_once('v').unwrap().0;
            assert_eq!(rest, format!("{run}v1h-{run}z"));
            let (x, y, run): (usize, usize, usize) =
                (x.parse().unwrap(), y.parse().unwrap(), run.parse().unwrap());
            for cell in &mut grid[y * size + x..y * size + x + run] {
                assert!(!*cell, "rectangles overlap");
                *cell = true;
            }
        }
        (size, grid)
    }

    #[test]
    fn the_svg_draws_exactly_the_codes_dark_modules_inside_a_quiet_zone() {
        let payload = "2@Xk3+aBc/dEf==,pubkey==,identity==,adv==";
        let code = QrCode::with_error_correction_level(payload, EcLevel::M).unwrap();
        let width = code.width();
        let (size, grid) = drawn(&svg(payload).unwrap());
        assert_eq!(size, width + 2 * QUIET_ZONE);
        let colors = code.to_colors();
        for y in 0..size {
            for x in 0..size {
                let inside = (QUIET_ZONE..QUIET_ZONE + width).contains(&x)
                    && (QUIET_ZONE..QUIET_ZONE + width).contains(&y);
                let dark =
                    inside && colors[(y - QUIET_ZONE) * width + (x - QUIET_ZONE)] == Color::Dark;
                assert_eq!(grid[y * size + x], dark, "module ({x}, {y})");
            }
        }
    }

    #[test]
    fn the_payload_never_reaches_the_markup() {
        let svg = svg("\"><script>alert(1)</script>").unwrap();
        assert!(!svg.contains("script"), "{svg}");
        assert!(svg.starts_with("<svg class=\"qr\""));
    }

    #[test]
    fn a_payload_too_long_for_a_qr_code_is_an_error() {
        assert_eq!(svg(&"x".repeat(4000)), Err(QrError::DataTooLong));
    }
}
