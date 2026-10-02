//! The Bufferless mark ("afterimage": a circle trailed by two fading echoes),
//! rasterized in software. Shared by build.rs, which bakes the exe icon, and
//! the app, which draws the tray icon at runtime. Geometry is in a 64x64 space.

/// (center x, radius, opacity) for each circle, back to front.
const CIRCLES: [(f32, f32, f32); 3] = [(21.0, 9.0, 0.16), (29.0, 10.2, 0.38), (38.5, 11.5, 1.0)];
const CIRCLE_Y: f32 = 32.0;

const TILE_RADIUS: f32 = 15.0;
const TILE_TOP_LEFT: [f32; 3] = [0x6a as f32 / 255.0, 0x5a as f32 / 255.0, 0xe0 as f32 / 255.0];
const TILE_BOTTOM_RIGHT: [f32; 3] = [0x1e as f32 / 255.0, 0x1a as f32 / 255.0, 0x4a as f32 / 255.0];

/// Samples per pixel along each axis.
const SUPERSAMPLE: u32 = 4;

/// Premultiplied RGBA.
type Color = [f32; 4];

fn over(dst: Color, rgb: [f32; 3], alpha: f32) -> Color {
    let k = 1.0 - alpha;
    [rgb[0] * alpha + dst[0] * k, rgb[1] * alpha + dst[1] * k, rgb[2] * alpha + dst[2] * k, alpha + dst[3] * k]
}

fn in_rounded_rect(u: f32, v: f32, inset: f32) -> bool {
    let (half, r) = (32.0 - inset, TILE_RADIUS - inset);
    let dx = ((u - 32.0).abs() - (half - r)).max(0.0);
    let dy = ((v - 32.0).abs() - (half - r)).max(0.0);
    dx * dx + dy * dy <= r * r
}

fn circles(mut c: Color, u: f32, v: f32, rgb: [f32; 3]) -> Color {
    for (cx, r, opacity) in CIRCLES {
        if (u - cx).powi(2) + (v - CIRCLE_Y).powi(2) <= r * r {
            c = over(c, rgb, opacity);
        }
    }
    c
}

/// Supersample `scene` over a `size` x `size` image covering the square
/// [origin, origin + span] of the 64-unit space. Returns straight-alpha RGBA8.
fn render(size: u32, origin: f32, span: f32, scene: impl Fn(f32, f32) -> Color) -> Vec<u8> {
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    let n = (SUPERSAMPLE * SUPERSAMPLE) as f32;
    for y in 0..size {
        for x in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let fx = x as f32 + (sx as f32 + 0.5) / SUPERSAMPLE as f32;
                    let fy = y as f32 + (sy as f32 + 0.5) / SUPERSAMPLE as f32;
                    let c = scene(origin + fx / size as f32 * span, origin + fy / size as f32 * span);
                    for i in 0..4 {
                        acc[i] += c[i];
                    }
                }
            }
            let a = acc[3] / n;
            let unpremul = |v: f32| if a > 0.0 { (v / n / a * 255.0).round().clamp(0.0, 255.0) as u8 } else { 0 };
            out.extend_from_slice(&[unpremul(acc[0]), unpremul(acc[1]), unpremul(acc[2]), (a * 255.0).round() as u8]);
        }
    }
    out
}

/// The app icon: the mark in white on an indigo rounded tile.
pub fn tile(size: u32) -> Vec<u8> {
    render(size, 0.0, 64.0, |u, v| {
        if !in_rounded_rect(u, v, 0.0) {
            return [0.0; 4];
        }
        let t = ((u + v) / 128.0).clamp(0.0, 1.0);
        let rgb = std::array::from_fn(|i| TILE_TOP_LEFT[i] + (TILE_BOTTOM_RIGHT[i] - TILE_TOP_LEFT[i]) * t);
        let mut c = over([0.0; 4], rgb, 1.0);
        // Faint inner edge highlight, like light catching the tile's rim.
        if !in_rounded_rect(u, v, 1.0) {
            c = over(c, [1.0; 3], 0.14);
        }
        circles(c, u, v, [1.0; 3])
    })
}

/// The bare mark for the tray, cropped tight and drawn in a single colour.
pub fn glyph(size: u32, rgb: [u8; 3], opacity: f32) -> Vec<u8> {
    let rgb = rgb.map(|c| c as f32 / 255.0);
    render(size, 8.0, 48.0, |u, v| {
        let c = circles([0.0; 4], u, v, rgb);
        c.map(|x| x * opacity)
    })
}
