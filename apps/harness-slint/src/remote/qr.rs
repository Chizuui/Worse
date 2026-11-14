//! Pair link → QR image (docs/plans/remote-contract.md "Pair link").
//!
//! The pair URL is `https://<public_host>/p#t=<token>`; the token sits in the fragment, so the tunnel edge
//! never sees it. Rendering is `qrcode` → RGBA matrix → `slint::Image` (module margin + integer scale, black on
//! white so a phone camera reads it on either theme).

/// Quiet zone around the matrix, in modules (QR spec: 4).
const MARGIN: usize = 4;
/// Pixels per module; 6 keeps a typical pair URL (~90 chars) inside a ~350 px image.
const SCALE: usize = 6;

/// QR image of `text`, or `None` when the text cannot be encoded (too long, invalid mode).
#[must_use]
pub fn image(text: &str) -> Option<slint::Image> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let modules = code.width();
    let side = (modules + 2 * MARGIN) * SCALE;
    let side_u32 = u32::try_from(side).ok()?;
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(side_u32, side_u32);
    let dark = slint::Rgba8Pixel {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };
    let light = slint::Rgba8Pixel {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };
    let pixels = buffer.make_mut_slice();
    pixels.fill(light);
    let colors = code.to_colors();
    for (index, color) in colors.iter().enumerate() {
        if *color != qrcode::types::Color::Dark {
            continue;
        }
        let row = index / modules;
        let col = index % modules;
        for dy in 0..SCALE {
            let y = (row + MARGIN) * SCALE + dy;
            for dx in 0..SCALE {
                let x = (col + MARGIN) * SCALE + dx;
                if let Some(pixel) = pixels.get_mut(y * side + x) {
                    *pixel = dark;
                }
            }
        }
    }
    Some(slint::Image::from_rgba8(buffer))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn renders_a_pair_link() {
        let image = image("https://harness.trycloudflare.com/p#t=00112233445566778899aabbccddeeff").expect("qr");
        assert!(image.size().width > 0 && image.size().width == image.size().height);
    }

    #[test]
    fn too_long_text_is_none_instead_of_panicking() {
        let text = "x".repeat(8000);
        assert!(image(&text).is_none());
    }
}
